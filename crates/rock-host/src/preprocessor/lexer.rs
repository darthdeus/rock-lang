use super::*;

/// Lexer for preprocessor directives
pub struct PreprocessorLexer<'a> {
    input: &'a str,
    bytes: &'a [u8],
    position: usize,
    line: usize,
    column: usize,
}

impl<'a> PreprocessorLexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            position: 0,
            line: 1,
            column: 1,
        }
    }

    /// Scan for the next preprocessor directive
    pub fn next_directive(&mut self) -> Option<Directive> {
        while self.position < self.bytes.len() {
            let pos_before = self.position;

            // Skip to start of line
            self.skip_to_line_start();

            // Skip whitespace at start of line
            self.skip_whitespace();

            // Check if this line starts with #
            if self.peek() == Some('#') {
                if let Some(directive) = self.parse_directive() {
                    return Some(directive);
                }
            }

            // Move to next line
            self.skip_to_next_line();

            // Check if we're stuck in a loop
            if self.position == pos_before {
                // If we're at the last line and there's no newline, advance to EOF
                if self.position < self.bytes.len() {
                    self.position = self.bytes.len();
                } else {
                    break;
                }
            }
        }

        None
    }

    /// Get all remaining content from current position
    pub fn remaining(&self) -> &str {
        &self.input[self.position..]
    }

    /// Get content between two positions
    pub fn slice(&self, start: usize, end: usize) -> &str {
        &self.input[start..end]
    }

    /// Get current position
    pub fn position(&self) -> usize {
        self.position
    }

    fn parse_directive(&mut self) -> Option<Directive> {
        let start_line = self.line;
        let start_column = self.column;
        let _start_pos = self.position;

        // Consume the #
        self.advance();

        // Skip whitespace
        self.skip_whitespace();

        // Read directive name
        let directive_name = self.read_identifier()?;

        let (directive_type, needs_condition) = match directive_name.as_str() {
            "if" => (DirectiveType::If, true),
            "elseif" => (DirectiveType::ElseIf, true),
            "else" => (DirectiveType::Else, false),
            "end" => (DirectiveType::End, false),
            "script" => (DirectiveType::Script, false),
            "test" => (DirectiveType::Test, false),
            "test-util" => (DirectiveType::TestUtil, false),
            "use" => (DirectiveType::Use, true), // Use needs a package name
            _ => return None, // Unknown directive
        };

        let condition = if needs_condition {
            self.skip_whitespace();
            Some(self.read_condition())
        } else {
            None
        };

        Some(Directive {
            directive_type,
            condition,
            line: start_line,
            column: start_column,
        })
    }

    fn read_identifier(&mut self) -> Option<String> {
        let start = self.position;

        while let Some(ch) = self.peek() {
            if ch.is_alphanumeric() || ch == '_' || ch == '-' {
                self.advance();
            } else {
                break;
            }
        }

        if start == self.position {
            None
        } else {
            Some(self.input[start..self.position].to_string())
        }
    }

    fn read_condition(&mut self) -> String {
        let start = self.position;

        // Read until end of line or comment
        while let Some(ch) = self.peek() {
            if ch == '\n' || (ch == '/' && self.peek_ahead(1) == Some('/')) {
                break;
            }
            self.advance();
        }

        self.input[start..self.position].trim().to_string()
    }

    fn skip_whitespace(&mut self) {
        while let Some(ch) = self.peek() {
            if ch == ' ' || ch == '\t' {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn skip_to_line_start(&mut self) {
        // If we're not at start of input and not after a newline, skip to next line
        if self.position > 0 && self.position < self.bytes.len() {
            // Get the previous character
            if self.position > 0 && self.bytes[self.position - 1] != b'\n' {
                self.skip_to_next_line();
            }
        }
    }

    fn skip_to_next_line(&mut self) {
        while let Some(ch) = self.peek() {
            self.advance();
            if ch == '\n' {
                break;
            }
        }
    }

    fn peek(&self) -> Option<char> {
        if self.position < self.bytes.len() {
            Some(self.bytes[self.position] as char)
        } else {
            None
        }
    }

    fn peek_ahead(&self, offset: usize) -> Option<char> {
        let pos = self.position + offset;
        if pos < self.bytes.len() {
            Some(self.bytes[pos] as char)
        } else {
            None
        }
    }

    fn advance(&mut self) {
        let Some(&byte) = self.bytes.get(self.position) else {
            return;
        };
        self.position += 1;
        (self.line, self.column) = if byte == b'\n' {
            (self.line + 1, 1)
        } else {
            (self.line, self.column + 1)
        };
    }
}
