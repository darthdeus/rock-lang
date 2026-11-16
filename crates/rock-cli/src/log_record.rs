//! Recognize actionable `rock_log` stamps in a child's stderr.

fn actionable_log_record(line: &str) -> Option<()> {
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
    // The time has width eight and exactly three fractional digits.
    let frac = padded[int_digits..].strip_prefix('.')?;
    let digits = frac.as_bytes().get(..3)?;
    if !digits.iter().all(u8::is_ascii_digit) || padding != 8usize.saturating_sub(int_digits + 4) {
        return None;
    }
    let tail = frac[3..].strip_prefix("s ")?;
    let (level, tail) = ["ERROR", "WARN "]
        .into_iter()
        .find_map(|tag| tail.strip_prefix(tag).map(|tail| (tag, tail)))?;
    if color_level.is_some_and(|color_level| color_level != level) {
        return None;
    }
    let tail = tail.strip_prefix(']')?;
    // Match the logger's separator even when the body is empty or multiline.
    tail.strip_prefix(rock_log::WALL_CLOCK_MARK)
        .or_else(|| tail.strip_prefix(' '))?;
    Some(())
}

pub(crate) fn is_error_or_warning_log_record(line: &str) -> bool {
    actionable_log_record(line).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(
                is_error_or_warning_log_record(header),
                level <= log::Level::Warn
            );
        }
    }
}
