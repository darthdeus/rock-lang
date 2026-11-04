//! skull-fmt: a Rock source formatter built on `skull-parser`.
//!
//! Pipeline: parse the source with `skull_parser::parse_module` (raw AST,
//! *never* cooked -- cook lowers surface syntax the formatter must preserve),
//! walk the AST into a `Doc` layout IR (`doc`), and render. Comments and blank
//! lines come from a separate `rock_tokenizer` pass attached by source position
//! (`trivia`), since the parser discards trailing/inline comments.
//!
//! Public surface mirrors `rock_format`: `format_code(&str) -> String`.

mod align;
mod doc;
mod printer;
mod trivia;

use rock_core::source_code::Dialect;
use ustr::Ustr;

#[derive(Clone, Debug)]
pub struct FormatConfig {
    pub indent_size: usize,
    pub max_line_length: usize,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            indent_size: 4,
            max_line_length: 100,
        }
    }
}

/// Format Rock source. On a parse error the input is returned unchanged (a
/// formatter must never emit corrupt or partial output); callers that need to
/// distinguish should use [`try_format`].
pub fn format_code(input: &str) -> String {
    try_format(input, &FormatConfig::default()).unwrap_or_else(|_| input.to_string())
}

/// Format with an explicit config, surfacing parse errors.
pub fn try_format(input: &str, cfg: &FormatConfig) -> Result<String, String> {
    try_format_with_dialect(input, cfg, Dialect::Legacy)
}

/// [`try_format`] with the source dialect selected: modules-v1 sources must
/// format under v1 rules so the parenthesized ascription round-trips.
pub fn try_format_with_dialect(
    input: &str,
    cfg: &FormatConfig,
    dialect: Dialect,
) -> Result<String, String> {
    let file: Ustr = "<fmt>".into();
    let ast = skull_parser::parse_module_with_dialect(input, file, dialect)
        .map_err(|e| format!("{e:?}"))?;
    if !ast.recovery_errors.is_empty() {
        return Err(format!("parse recovery errors: {:?}", ast.recovery_errors));
    }

    let trivia = trivia::Trivia::harvest(input, file)?;
    let printer = printer::Printer::new(&ast, input, cfg, &trivia);
    let document = printer.module();
    let rendered = doc::render(&document, cfg.max_line_length, cfg.indent_size);
    let mut out = align::align(&rendered);

    // A formatted file ends in exactly one newline.
    while out.ends_with('\n') {
        out.pop();
    }
    while out.ends_with(char::is_whitespace) {
        out.pop();
    }
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{FormatConfig, try_format};

    #[test]
    fn item_annotations_preserve_trailing_comments() {
        let cases = [
            "@sim_env(\"SIM_DRAW\", \"every_frame\") // draw policy\n@sim\nco fn probe() {\n}\n",
            "@sim // final annotation\nco fn probe() {\n}\n",
            "@first @second // shared line\nfn probe() {\n}\n",
        ];
        for source in cases {
            let formatted = try_format(source, &FormatConfig::default()).expect("format fixture");
            assert_eq!(formatted, source);
            skull_parser::parse_module_clean(&formatted, "<annotation-comment>".into())
                .expect("formatted annotations must parse");
            assert_eq!(
                try_format(&formatted, &FormatConfig::default()).unwrap(),
                formatted
            );
        }
    }

    #[test]
    fn trailing_block_lambda_keeps_call_layout_after_expansion() {
        let config = FormatConfig {
            max_line_length: 20,
            ..FormatConfig::default()
        };
        let source = "fn probe() {\n    check(() => { touch() })\n}\n";
        let once = try_format(source, &config).expect("format lambda");
        let twice = try_format(&once, &config).expect("format expanded lambda");
        assert_eq!(once, twice);

        let separated =
            "fn probe() {\n    check(\n        () => {\n            touch()\n        }\n    )\n}\n";
        assert_eq!(try_format(separated, &config).unwrap(), separated);
    }
}
