// The parser output is never cooked: cooking lowers surface syntax the formatter must preserve.
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
    try_format_named(input, cfg, "<fmt>")
}

/// [`try_format`] with the source's real name in parse diagnostics.
pub fn try_format_named(
    input: &str,
    cfg: &FormatConfig,
    file_name: &str,
) -> Result<String, String> {
    let file: Ustr = file_name.into();
    let (ast, (trivia, before)) = skull_parser::parse_module_with_tokens(input, file, |tokens| {
        (
            trivia::Trivia::harvest(input, tokens),
            comments(input, tokens),
        )
    })
    .map_err(|e| e.one_line())?;
    if !ast.recovery_errors.is_empty() {
        let lines: Vec<String> = ast.recovery_errors.iter().map(|e| e.one_line()).collect();
        return Err(lines.join("\n"));
    }

    let printer = printer::Printer::new(&ast, input, cfg, &trivia);
    let document = printer.module();
    let rendered = doc::render(&document, cfg.max_line_length, cfg.indent_size);
    let mut out = align::align(&rendered);

    while out.ends_with('\n') {
        out.pop();
    }
    while out.ends_with(char::is_whitespace) {
        out.pop();
    }
    out.push('\n');
    // The printer places comments by position and can miss one; never hand
    // back output that lost or duplicated a comment.
    let tokens = rock_tokenizer::Tokenizer::new(&out, file)
        .tokenize()
        .map_err(|e| format!("{e:?}"))?;
    check_comments(&before, &comments(&out, &tokens))?;
    Ok(out)
}

fn check_comments(before: &[String], after: &[String]) -> Result<(), String> {
    if before != after {
        let lost: Vec<&String> = before.iter().filter(|c| !after.contains(c)).collect();
        return Err(format!(
            "formatting would not keep every comment (lost {lost:?}); file left unformatted"
        ));
    }
    Ok(())
}

fn comments(src: &str, tokens: &[rock_tokenizer::Token]) -> Vec<String> {
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
    out
}

#[cfg(test)]
mod tests {
    use super::{FormatConfig, check_comments, try_format};

    #[test]
    fn a_missing_comment_fails_the_format() {
        let err = check_comments(&["// c".into()], &[]).expect_err("comment dropped");
        assert!(err.contains("// c"), "{err}");
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
