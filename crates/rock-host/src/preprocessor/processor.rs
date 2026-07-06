use super::*;
use parser::{ConditionalConstruct, PreprocessorParser};

/// Fast check if a source file contains any preprocessor directives
/// This is much faster than full lexing as it just looks for '#' at line starts
fn contains_preprocessor_directives(input: &str) -> bool {
    // Check if the input starts with '#' (for directives at the very beginning)
    if input.starts_with('#') {
        return true;
    }
    
    // Look for '\n#' pattern (directive at start of any other line)
    // This is a simple string search, much faster than character-by-character lexing
    input.contains("\n#")
}

/// Check for special directives and return the file type and imports
fn check_for_special_directives(input: &str) -> (bool, bool, bool, Vec<String>) {
    let mut lexer = lexer::PreprocessorLexer::new(input);
    let mut is_script = false;
    let mut is_test = false;
    let mut is_test_util = false;
    let mut used_packages = Vec::new();

    // Look for special directives (usually at the beginning)
    while let Some(directive) = lexer.next_directive() {
        match directive.directive_type {
            DirectiveType::Script => is_script = true,
            DirectiveType::Test => is_test = true,
            DirectiveType::TestUtil => is_test_util = true,
            DirectiveType::Use => {
                if let Some(package_name) = directive.condition {
                    let package_name = package_name.trim();
                    // Remove quotes if present
                    let package_name = if (package_name.starts_with('"') && package_name.ends_with('"')) ||
                                       (package_name.starts_with('\'') && package_name.ends_with('\'')) {
                        package_name[1..package_name.len()-1].to_string()
                    } else {
                        package_name.to_string()
                    };
                    used_packages.push(package_name);
                }
            }
            _ => continue,
        }
    }

    (is_script, is_test, is_test_util, used_packages)
}

/// Preprocess source code based on the backend context
pub fn preprocess(input: &str, context: &PreprocessorContext) -> Result<PreprocessedSource> {
    // Fast path: Check if the file contains any potential preprocessor directives
    // by looking for '#' at the beginning of lines (much faster than full lexing)
    if !contains_preprocessor_directives(input) {
        // No preprocessing needed
        return Ok(PreprocessedSource {
            content: input.to_string(),
            is_script: false,
            is_test: false,
            is_test_util: false,
            used_packages: vec![],
        });
    }

    // Check for special directives at the beginning of the file
    let (is_script, is_test, is_test_util, used_packages) = check_for_special_directives(input);

    let mut parser = PreprocessorParser::new();
    let constructs = parser.parse_directives(input)?;

    if constructs.is_empty() && !is_script && !is_test && !is_test_util && used_packages.is_empty() {
        // No preprocessing needed (shouldn't happen after fast check, but keep for safety)
        return Ok(PreprocessedSource {
            content: input.to_string(),
            is_script: false,
            is_test: false,
            is_test_util: false,
            used_packages: vec![],
        });
    }

    // Process the source
    let mut output = String::new();
    let mut last_pos = 0;

    // Sort constructs by position
    let mut sorted_constructs = constructs;
    sorted_constructs.sort_by_key(|c| c.blocks[0].start_pos);

    for construct in sorted_constructs {
        let construct_start = find_line_start(input, construct.blocks[0].start_pos);
        let construct_end = find_line_end(
            input,
            construct
                .else_block
                .as_ref()
                .map(|b| b.end_pos)
                .unwrap_or_else(|| construct.blocks.last().unwrap().end_pos),
        );

        // Copy content before this construct
        let before = &input[last_pos..construct_start];
        output.push_str(before);

        // Find which block to include
        let block_to_include = find_matching_block(&construct, context);

        if let Some(block) = block_to_include {
            // Extract the content of this block (excluding the directive lines)
            let block_content_start = find_line_end(input, block.start_pos);
            let block_content_end = find_line_start(input, block.end_pos);

            if block_content_start < block_content_end {
                let block_content = &input[block_content_start..block_content_end];
                output.push_str(block_content);
            }
        }

        last_pos = construct_end;
    }

    // Copy remaining content
    if last_pos < input.len() {
        let remaining = &input[last_pos..];
        output.push_str(remaining);
    }

    Ok(PreprocessedSource {
        content: output,
        is_script,
        is_test,
        is_test_util,
        used_packages,
    })
}

fn find_matching_block<'a>(
    construct: &'a ConditionalConstruct,
    context: &PreprocessorContext,
) -> Option<&'a parser::ConditionalBlock> {
    // Check each conditional block in order
    for block in &construct.blocks {
        if let Some(condition) = &block.condition {
            if context.evaluate_condition(condition) {
                return Some(block);
            }
        }
    }

    // If no condition matched, use else block if present
    construct.else_block.as_ref()
}

fn find_line_start(input: &str, pos: usize) -> usize {
    let mut start = pos;
    while start > 0 && input.as_bytes()[start - 1] != b'\n' {
        start -= 1;
    }
    start
}

fn find_line_end(input: &str, pos: usize) -> usize {
    let mut end = pos;
    while end < input.len() && input.as_bytes()[end] != b'\n' {
        end += 1;
    }
    if end < input.len() {
        end += 1; // Include the newline
    }
    end
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_if() {
        let input = r#"fn main() {
#if lua
    print("Lua backend")
#end
}"#;

        let context = PreprocessorContext::new("lua");
        let result = preprocess(input, &context).unwrap();

        assert!(result.content.contains("Lua backend"));
        assert!(!result.content.contains("#if"));
        assert!(!result.content.contains("#end"));
    }

    #[test]
    fn test_if_else() {
        let input = r#"fn main() {
#if lua
    print("Lua backend")
#else
    print("Other backend")
#end
}"#;

        let context = PreprocessorContext::new("lua");
        let result = preprocess(input, &context).unwrap();

        assert!(result.content.contains("Lua backend"));
        assert!(!result.content.contains("Other backend"));
    }
}
