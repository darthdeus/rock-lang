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

    pub(crate) fn loc_per_sec(&self, elapsed: std::time::Duration) -> f64 {
        let secs = elapsed.as_secs_f64();
        if secs <= 0.0 {
            0.0
        } else {
            self.total_lines as f64 / secs
        }
    }

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
