use regex_lite::Regex;

/// Strip ANSI color codes and Unicode box-drawing characters from error messages
/// to make them display properly in HTML or plain text contexts
pub fn strip_ansi_and_unicode(text: &str) -> String {
    // Remove ANSI color codes (like [31m, [0m, [38;5;246m, etc.)
    let ansi_regex = Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    let without_ansi = ansi_regex.replace_all(text, "");

    // Replace Unicode box-drawing characters with ASCII equivalents
    let replacements = [
        ('\u{256D}', "+"), ('\u{256F}', "+"), ('\u{2570}', "+"),
        ('\u{253C}', "+"), ('\u{2524}', "+"), ('\u{251C}', "+"),
        ('\u{2510}', "+"), ('\u{2518}', "+"), ('\u{2514}', "+"),
        ('\u{250C}', "+"), ('\u{252C}', "+"),
        ('\u{2500}', "-"), ('\u{2502}', "|")
    ];

    replacements.iter().fold(without_ansi.to_string(), |acc, &(from, to)| {
        acc.replace(from, to)
    })
}
