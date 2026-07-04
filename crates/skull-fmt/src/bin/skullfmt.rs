//! Minimal CLI: `skullfmt <file.rock>` prints the formatted source to stdout.
//! A stepping stone toward wiring skull-fmt into `rock fmt`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: skullfmt <file>");
        return ExitCode::FAILURE;
    };
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match skull_fmt::try_format(&src, &skull_fmt::FormatConfig::default()) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{path}: {e}");
            ExitCode::FAILURE
        }
    }
}
