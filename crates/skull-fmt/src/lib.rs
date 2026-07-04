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
    let file: Ustr = "<fmt>".into();
    let ast = skull_parser::parse_module(input, file).map_err(|e| format!("{e:?}"))?;
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
