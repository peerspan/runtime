// SPDX-License-Identifier: Apache-2.0
// Copyright The Peerspan Authors

//! Messages to whoever runs the runtime, on standard error.
//!
//! `run` writes them for a person at a terminal: `peerspan-runtime: message`. `serve` switches
//! to one line per event for an operator and the journal:
//! `2026-10-28T06:40:12.345Z info run finished run=run_… exit_code=0`, with values quoted when
//! they hold spaces.

use std::fmt::{Display, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static STRUCTURED: AtomicBool = AtomicBool::new(false);

/// Switches to timestamped lines with fields, as `serve` writes them.
pub fn structured() {
    STRUCTURED.store(true, Ordering::Relaxed);
}

/// A field's name and value.
pub type Field<'a> = (&'a str, &'a dyn Display);

/// Something that happened, as expected.
pub fn info(message: &str, fields: &[Field<'_>]) {
    write("info", message, fields);
}

/// Something that went wrong without stopping the runtime.
pub fn warn(message: &str, fields: &[Field<'_>]) {
    write("warn", message, fields);
}

fn write(level: &str, message: &str, fields: &[Field<'_>]) {
    let line = if STRUCTURED.load(Ordering::Relaxed) {
        let mut line = format!("{} {level} {message}", rfc3339_millis(SystemTime::now()));
        for (name, value) in fields {
            let _ = write!(line, " {name}={}", quote(&value.to_string()));
        }
        line
    } else {
        let mut line = format!("peerspan-runtime: {message}");
        if !fields.is_empty() {
            let pairs: Vec<String> = fields
                .iter()
                .map(|(name, value)| format!("{name}={}", quote(&value.to_string())))
                .collect();
            let _ = write!(line, " ({})", pairs.join(" "));
        }
        line
    };
    eprintln!("{line}");
}

/// The value as is, or in double quotes with `"` and `\` escaped if it is empty or holds
/// spaces, quotes, `=` or control characters, so each line splits back into its fields.
fn quote(value: &str) -> String {
    let plain = !value.is_empty()
        && !value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '=' | '\\'));
    if plain {
        return value.to_owned();
    }
    let mut quoted = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            c if c.is_control() => {
                let _ = write!(quoted, "\\u{{{:x}}}", u32::from(c));
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// `time` in UTC as RFC 3339, to the second: `2026-10-27T08:34:56Z`.
pub fn rfc3339(time: SystemTime) -> String {
    let secs = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// `time` in UTC as RFC 3339, to the millisecond: `2026-10-27T08:34:56.789Z`.
fn rfc3339_millis(time: SystemTime) -> String {
    let millis = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_millis());
    let seconds = rfc3339(time);
    format!("{}.{millis:03}Z", &seconds[..seconds.len() - 1])
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn rfc3339_formats_known_instants() {
        let at = |secs| rfc3339(UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(at(1_793_090_096), "2026-10-27T08:34:56Z");
        assert_eq!(
            rfc3339_millis(UNIX_EPOCH + Duration::from_millis(1_793_090_096_007)),
            "2026-10-27T08:34:56.007Z"
        );
    }

    #[test]
    fn values_are_quoted_only_when_they_must_be() {
        assert_eq!(quote("run_ab12"), "run_ab12");
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote("no such file"), "\"no such file\"");
        assert_eq!(quote("a=b \"c\"\n"), "\"a=b \\\"c\\\"\\n\"");
    }
}
