//! Parse the stamp `rock_log` puts on every record, so a child's stderr can be
//! filtered by severity.

/// The five level tags `rock_log` emits, each exactly five characters
/// (rock-log's `level_tag`). WARN and INFO are space-padded
/// to that width, which is why the closing bracket can look detached.
const LOG_LEVEL_TAGS: [&str; 5] = ["ERROR", "WARN ", "INFO ", "DEBUG", "TRACE"];
const LOG_TIME_WIDTH: usize = 8;
const LOG_TIME_PRECISION: usize = 3;

/// A `rock_log` record's `(level, body after the stamp)`.
///
/// Matches the emitter's grammar exactly -- `[{:>8.3}s {}]{sep}` -- rather
/// than sniffing for something bracketed: leading padding, at least one
/// integer digit, a point, EXACTLY three fractional digits, `s`, one space,
/// one of [`LOG_LEVEL_TAGS`], `]`, and a separator that is either a space or
/// [`rock_log::WALL_CLOCK_MARK`]. A loose match reads a game's own bracketed
/// prefix as a log level: `[1.23s SCORE] result` is game output, and both its
/// fractional precision and its tag say so. A `print()` that reproduces the
/// grammar byte for byte still reads as a log record.
fn parse_log_record(line: &str) -> Option<(&'static str, &str)> {
    // emit wraps the whole message; multiline headers have no reset until
    // the last continuation line, while callers can pass individual lines.
    let (line, color_level) = if let Some(line) = line.strip_prefix("\x1b[1;31m") {
        (line.strip_suffix("\x1b[0m").unwrap_or(line), Some("ERROR"))
    } else if let Some(line) = line.strip_prefix("\x1b[33m") {
        (line.strip_suffix("\x1b[0m").unwrap_or(line), Some("WARN "))
    } else {
        (line, None)
    };
    let rest = line.strip_prefix('[')?;
    let padded = rest.trim_start_matches(' ');
    let padding = rest.len() - padded.len();
    let int_digits = padded
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(padded.len());
    if int_digits == 0 {
        return None;
    }
    let frac = padded[int_digits..].strip_prefix('.')?;
    if frac.len() < LOG_TIME_PRECISION
        || !frac.as_bytes()[..LOG_TIME_PRECISION]
            .iter()
            .all(u8::is_ascii_digit)
    {
        return None;
    }
    let number_width = int_digits + 1 + LOG_TIME_PRECISION;
    if padding != LOG_TIME_WIDTH.saturating_sub(number_width) {
        return None;
    }
    // A fourth fractional digit fails this, which is what pins the precision.
    let tail = frac[LOG_TIME_PRECISION..].strip_prefix("s ")?;
    let (level, tail) = LOG_LEVEL_TAGS
        .iter()
        .find_map(|tag| tail.strip_prefix(tag).map(|tail| (*tag, tail)))?;
    if color_level.is_some_and(|color_level| color_level != level) {
        return None;
    }
    let tail = tail.strip_prefix(']')?;
    let body = match tail.strip_prefix(rock_log::WALL_CLOCK_MARK) {
        Some(body) => body,
        None => tail.strip_prefix(' ')?,
    };
    Some((level, body))
}

pub fn is_error_or_warning_log_record(line: &str) -> bool {
    parse_log_record(line).is_some_and(|(level, _)| matches!(level, "ERROR" | "WARN "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shape `rock_log` actually emits parses to its body, including the
    /// padded `WARN `/`INFO ` tags, whose padding makes the closing bracket
    /// look detached.
    #[test]
    fn every_real_log_stamp_parses() {
        for (line, expected) in [
            ("[   0.789s INFO ] [GAME] hi", "[GAME] hi"),
            (
                "[   1.296s ERROR] [SLOWFRAME] frame 60 took 69.7ms",
                "[SLOWFRAME] frame 60 took 69.7ms",
            ),
            ("[   0.001s WARN ] [SIM] careful", "[SIM] careful"),
            (
                "\x1b[1;31m[   1.296s ERROR] [PERF_TEST] failure\x1b[0m",
                "[PERF_TEST] failure",
            ),
            (
                "\x1b[33m[   0.001s WARN ] [PERF_TEST] caution\x1b[0m",
                "[PERF_TEST] caution",
            ),
            ("[  12.500s DEBUG] [ENGINE] x", "[ENGINE] x"),
            (
                "[1234.567s TRACE] no padding, wide number",
                "no padding, wide number",
            ),
            ("[   0.000s INFO ] ", ""),
        ] {
            let parsed =
                parse_log_record(line).unwrap_or_else(|| panic!("`{line}` is a real logger stamp"));
            assert_eq!(parsed.1, expected, "`{line}`");
        }
    }

    /// The stamp is parsed, not sniffed. Everything here merely RESEMBLES one;
    /// `[1.23s SCORE]` is the canonical trap: bracketed, numeric, ends in `s`,
    /// and is not a log record.
    #[test]
    fn only_a_real_log_stamp_parses() {
        for line in [
            "[GAME] gate: no spawners",
            "[1.23s SCORE] result",
            "[   0.789s SCORE] a well-formed time with a tag we never emit",
            "[   0.789s Info ] the tags are upper case",
            "[   0.78s INFO ] two fractional digits",
            "[   0.7891s INFO ] four fractional digits",
            "[   0.789 INFO ] no seconds marker",
            "[   0.789sINFO ] no space after the seconds",
            "[   0.789s INFO] the tag is padded, so this is one char short",
            "[    0.789s INFO ] too much width padding",
            "[  0.789s INFO ] too little width padding",
            "[   0.789s ERROR]no separator after the bracket",
            "[   .789s INFO ] no integer digits",
            "[   0.789s] no level at all",
            "[0.5] a bracketed number that is not a stamp",
            "not bracketed at all",
            "",
        ] {
            assert!(
                parse_log_record(line).is_none(),
                "`{line}` is not a logger stamp"
            );
        }
    }

    #[test]
    fn colored_actionable_log_levels_use_the_shared_stamp_parser() {
        assert!(is_error_or_warning_log_record(
            "\x1b[1;31m[   1.296s ERROR] [PERF_TEST] failure\x1b[0m"
        ));
        assert!(is_error_or_warning_log_record(
            "\x1b[33m[   0.001s WARN ] [PERF_TEST] caution\x1b[0m"
        ));
        assert!(!is_error_or_warning_log_record(
            "[   0.789s INFO ] [PERF_TEST] info"
        ));
        assert!(!is_error_or_warning_log_record(
            "\x1b[33m[   1.296s ERROR] [PERF_TEST] wrong color\x1b[0m"
        ));
    }

    #[test]
    fn production_multiline_logger_helper() {
        if std::env::var_os("ROCK_PARSER_LOG_HELPER").is_none() {
            return;
        }
        rock_log::init();
        for level in [log::Level::Error, log::Level::Warn, log::Level::Info] {
            log::log!(target: "PARSER_TEST", level, "diagnostic header\ndiagnostic continuation");
        }
    }

    #[test]
    fn shared_parser_accepts_production_colored_multiline_output() {
        // Isolate logger configuration and record tables in the helper process.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "log_record::tests::production_multiline_logger_helper",
                "--exact",
                "--nocapture",
            ])
            .env("ROCK_PARSER_LOG_HELPER", "1")
            .env("ROCK_LOG_COLOR", "always")
            .env("ROCK_LOG", "info")
            .output()
            .expect("production logger helper");
        assert!(output.status.success(), "logger helper failed: {output:?}");
        let emitted = String::from_utf8(output.stderr).unwrap();
        let lines: Vec<_> = emitted.lines().collect();
        let levels = [log::Level::Error, log::Level::Warn, log::Level::Info];
        assert_eq!(
            lines.len(),
            levels.len() * 2,
            "incomplete logger output: {emitted:?}"
        );
        for (record, level) in lines.as_chunks::<2>().0.iter().zip(levels) {
            let header = record[0];
            let parsed = parse_log_record(header)
                .unwrap_or_else(|| panic!("production header rejected: {header:?}"));
            assert_eq!(parsed.0.trim_end(), level.as_str());
            let body = header
                .split_once("] ")
                .expect("production stamp separator")
                .1;
            assert_eq!(parsed.1, body);
            assert_eq!(
                is_error_or_warning_log_record(header),
                level <= log::Level::Warn
            );
        }
    }
}
