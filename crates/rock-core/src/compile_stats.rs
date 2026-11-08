/// Aggregate LoC stats for one compile. Filled by the tokenizer as a
/// byproduct of parsing (see `rock_tokenizer::count_source_stats`) and
/// surfaced alongside compile-time prints as a `loc/sec` figure
/// comparable to https://wiki.alopex.li/LanguageCompilationSpeed.
///
/// Per-file `CompileStats` values are produced during the per-module
/// parse loop and `merge`'d into the storage-level aggregate. Counting
/// piggybacks on the existing tokenizer pass — no extra byte walk —
/// and uses the tokenizer's notion of comment vs. string-literal, so a
/// `//` or `/*` inside a string is correctly classified as code.
#[derive(Debug, Default, Clone, Copy)]
pub struct CompileStats {
    pub files: usize,
    pub total_lines: usize,
    pub code_lines: usize,
    pub comment_lines: usize,
    pub blank_lines: usize,
    pub bytes: usize,
}

impl CompileStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Headline lines-per-second: tokei-compatible `total_lines / elapsed`.
    /// Returns 0.0 if elapsed is zero (avoids div-by-zero in tests).
    pub fn loc_per_sec(&self, elapsed: std::time::Duration) -> f64 {
        let secs = elapsed.as_secs_f64();
        if secs <= 0.0 {
            0.0
        } else {
            self.total_lines as f64 / secs
        }
    }

    /// Render the headline number as `47.3k loc/sec` (or `847 loc/sec`
    /// below 1000). One canonical formatter so every print site looks
    /// the same.
    pub fn format_throughput(&self, elapsed: std::time::Duration) -> String {
        let lps = self.loc_per_sec(elapsed);
        if lps >= 10_000.0 {
            format!("{:.1}k loc/sec", lps / 1000.0)
        } else if lps >= 1000.0 {
            format!("{:.2}k loc/sec", lps / 1000.0)
        } else {
            format!("{:.0} loc/sec", lps)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn loc_per_sec_zero_elapsed() {
        let s = CompileStats {
            total_lines: 1000,
            ..Default::default()
        };
        assert_eq!(s.loc_per_sec(Duration::ZERO), 0.0);
    }

    #[test]
    fn loc_per_sec_basic() {
        let s = CompileStats {
            total_lines: 1000,
            ..Default::default()
        };
        let rate = s.loc_per_sec(Duration::from_secs(1));
        assert!((rate - 1000.0).abs() < 0.01);
    }

    #[test]
    fn format_throughput_units() {
        let s = CompileStats {
            total_lines: 50_000,
            ..Default::default()
        };
        assert_eq!(s.format_throughput(Duration::from_secs(1)), "50.0k loc/sec");

        let s = CompileStats {
            total_lines: 2_500,
            ..Default::default()
        };
        assert_eq!(s.format_throughput(Duration::from_secs(1)), "2.50k loc/sec");

        let s = CompileStats {
            total_lines: 100,
            ..Default::default()
        };
        assert_eq!(s.format_throughput(Duration::from_secs(1)), "100 loc/sec");
    }
}
