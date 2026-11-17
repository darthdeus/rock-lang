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
    try_format_with_dialect(input, cfg, Dialect::V1)
}

/// [`try_format`] with a compatibility dialect argument. Parsing always uses
/// the modules-v1 grammar.
pub fn try_format_with_dialect(
    input: &str,
    cfg: &FormatConfig,
    _dialect: Dialect,
) -> Result<String, String> {
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
    // The printer places comments by position and can miss one; never hand
    // back output that lost or duplicated a comment.
    let (before, after) = (comments(input, file)?, comments(&out, file)?);
    if before != after {
        let lost: Vec<&String> = before.iter().filter(|c| !after.contains(c)).collect();
        return Err(format!(
            "formatting would not keep every comment (lost {lost:?}); file left unformatted"
        ));
    }
    Ok(out)
}

/// The comments of `src` as `//`/`/* */` source text, trimmed and sorted.
fn comments(src: &str, file: Ustr) -> Result<Vec<String>, String> {
    let tokens = rock_tokenizer::Tokenizer::new(src, file)
        .tokenize()
        .map_err(|e| format!("{e:?}"))?;
    let mut out: Vec<String> = tokens
        .iter()
        .filter_map(|t| match t.kind {
            rock_tokenizer::TokenKind::Comment(_) => {
                let (lo, hi) = t.span.offset_range();
                Some(src[lo..hi].trim().to_string())
            }
            _ => None,
        })
        .collect();
    out.sort_unstable();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{FormatConfig, format_code, try_format};

    #[test]
    fn a_comment_the_printer_would_drop_fails_the_format() {
        let source = "fn f() {\n    let v = (\n        a,\n        // c\n        b\n    )\n}\n";
        let err = try_format(source, &FormatConfig::default()).expect_err("comment dropped");
        assert!(err.contains("// c"), "{err}");
        assert_eq!(format_code(source), source);
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
