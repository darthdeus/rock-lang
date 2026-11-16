//! Lexical references in opaque JavaScript bodies.

#[derive(Clone, Copy)]
enum Token<'a> {
    Name(&'a str),
    Punct(u8),
    Literal,
}

pub(crate) fn bare_names(text: &str) -> impl Iterator<Item = &str> {
    let mut tokens = Vec::new();
    let mut cursor = 0;
    scan(text, &mut cursor, false, &mut tokens);
    let mut names = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let Token::Name(name) = token else { continue };
        let property = matches!(tokens.get(i.wrapping_sub(1)), Some(Token::Punct(b'.')))
            || (matches!(
                tokens.get(i.wrapping_sub(1)),
                Some(Token::Punct(b'{' | b','))
            ) && (matches!(tokens.get(i + 1), Some(Token::Punct(b':')))
                || method_name(&tokens[i + 1..])));
        if !property {
            names.push(*name);
        }
    }
    names.into_iter()
}

fn method_name(tokens: &[Token<'_>]) -> bool {
    if !matches!(tokens.first(), Some(Token::Punct(b'('))) {
        return false;
    }
    let mut depth = 0;
    for (i, token) in tokens.iter().enumerate() {
        match token {
            Token::Punct(b'(') => depth += 1,
            Token::Punct(b')') => {
                depth -= 1;
                if depth == 0 {
                    return matches!(tokens.get(i + 1), Some(Token::Punct(b'{')));
                }
            }
            _ => {}
        }
    }
    false
}

fn skip_quoted(bytes: &[u8], i: &mut usize, quote: u8) {
    *i += 1;
    while *i < bytes.len() {
        match bytes[*i] {
            b'\\' => *i = (*i + 2).min(bytes.len()),
            c if c == quote => {
                *i += 1;
                break;
            }
            _ => *i += 1,
        }
    }
}

fn scan<'a>(text: &'a str, i: &mut usize, template_expr: bool, tokens: &mut Vec<Token<'a>>) {
    let bytes = text.as_bytes();
    let mut braces = 0;
    while *i < bytes.len() {
        match bytes[*i] {
            c if c.is_ascii_whitespace() => *i += 1,
            b'/' if bytes.get(*i + 1) == Some(&b'/') => {
                while *i < bytes.len() && bytes[*i] != b'\n' {
                    *i += 1;
                }
            }
            b'/' if bytes.get(*i + 1) == Some(&b'*') => {
                *i += 2;
                while *i < bytes.len() && !bytes[*i..].starts_with(b"*/") {
                    *i += 1;
                }
                *i = (*i + 2).min(bytes.len());
            }
            quote @ (b'\'' | b'"') => {
                skip_quoted(bytes, i, quote);
                tokens.push(Token::Literal);
            }
            b'`' => {
                *i += 1;
                tokens.push(Token::Literal);
                while *i < bytes.len() {
                    match bytes[*i] {
                        b'\\' => *i = (*i + 2).min(bytes.len()),
                        b'`' => {
                            *i += 1;
                            break;
                        }
                        b'$' if bytes.get(*i + 1) == Some(&b'{') => {
                            *i += 2;
                            tokens.push(Token::Punct(b'('));
                            scan(text, i, true, tokens);
                            tokens.push(Token::Punct(b')'));
                        }
                        _ => *i += 1,
                    }
                }
                tokens.push(Token::Literal);
            }
            b'/' if regex_start(tokens.last()) => {
                *i += 1;
                let mut class = false;
                while *i < bytes.len() {
                    match bytes[*i] {
                        b'\\' => {
                            *i = (*i + 2).min(bytes.len());
                            continue;
                        }
                        b'[' => class = true,
                        b']' => class = false,
                        b'/' if !class => {
                            *i += 1;
                            break;
                        }
                        _ => {}
                    }
                    *i += 1;
                }
                while *i < bytes.len() && bytes[*i].is_ascii_alphabetic() {
                    *i += 1;
                }
                tokens.push(Token::Literal);
            }
            c if c.is_ascii_alphabetic() || matches!(c, b'_' | b'$') || !c.is_ascii() => {
                let start = *i;
                *i += 1;
                while *i < bytes.len()
                    && (bytes[*i].is_ascii_alphanumeric()
                        || matches!(bytes[*i], b'_' | b'$')
                        || !bytes[*i].is_ascii())
                {
                    *i += 1;
                }
                tokens.push(Token::Name(&text[start..*i]));
            }
            b'}' if template_expr && braces == 0 => {
                *i += 1;
                return;
            }
            c => {
                if c == b'{' {
                    braces += 1;
                }
                if c == b'}' {
                    braces -= 1;
                }
                tokens.push(Token::Punct(c));
                *i += 1;
            }
        }
    }
}

fn regex_start(previous: Option<&Token<'_>>) -> bool {
    matches!(
        previous,
        None | Some(Token::Punct(
            b'(' | b'[' | b'{' | b'=' | b',' | b':' | b';' | b'!' | b'?' | b'&' | b'|'
        ))
    ) || matches!(
        previous,
        Some(Token::Name(
            "return" | "throw" | "case" | "typeof" | "void" | "delete" | "yield"
        ))
    )
}

#[cfg(test)]
mod tests {
    use super::bare_names;

    #[test]
    fn references_skip_properties_and_literal_text() {
        let source = r#"
            obj /* gap */ . helper;
            obj?.helper;
            ({helper: 1, helper() {}});
            '$helper helper'; /helper[\\/]/g;
            `helper ${helper()} ${`raw ${other()}`}`;
            // helper
            /* helper */
            $helper; helper_suffix; helper;
            condition ? helper : other;
            (1, helper());
        "#;
        let names: Vec<_> = bare_names(source)
            .filter(|n| *n == "helper" || *n == "other")
            .collect();
        assert_eq!(
            names,
            ["helper", "other", "helper", "helper", "other", "helper"]
        );
    }
}
