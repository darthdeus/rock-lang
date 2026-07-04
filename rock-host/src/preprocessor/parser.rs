use super::*;

/// A block of code with its condition
#[derive(Debug)]
pub struct ConditionalBlock {
    pub condition: Option<String>,
    pub start_pos: usize,
    pub end_pos: usize,
    pub start_line: usize,
}

/// A complete conditional construct (#if...#end)
#[derive(Debug)]
pub struct ConditionalConstruct {
    pub blocks: Vec<ConditionalBlock>,
    pub else_block: Option<ConditionalBlock>,
}

/// Parser for preprocessor directives
pub struct PreprocessorParser {
    directives: Vec<(Directive, usize)>, // (directive, position in source)
}

impl PreprocessorParser {
    pub fn new() -> Self {
        Self {
            directives: Vec::new(),
        }
    }

    /// Parse all directives and their positions from the lexer
    pub fn parse_directives(&mut self, input: &str) -> Result<Vec<ConditionalConstruct>> {
        let mut lexer = lexer::PreprocessorLexer::new(input);

        // Collect all directives with their positions
        while let Some(directive) = lexer.next_directive() {
            let pos = lexer.position();
            self.directives.push((directive, pos));
        }

        // Parse the directive structure
        self.parse_constructs()
    }

    fn parse_constructs(&mut self) -> Result<Vec<ConditionalConstruct>> {
        let mut constructs = Vec::new();
        let mut i = 0;

        while i < self.directives.len() {
            let (directive, _) = &self.directives[i];

            match directive.directive_type {
                DirectiveType::If => {
                    let construct = self.parse_conditional_construct(i)?;
                    i = construct.1; // Skip to after the #end
                    constructs.push(construct.0);
                }
                DirectiveType::Script | DirectiveType::Test | DirectiveType::TestUtil | DirectiveType::Use => {
                    // Skip script/test/use directives as they are handled separately
                    i += 1;
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "Unexpected directive {:?} at line {}",
                        directive.directive_type,
                        directive.line
                    ));
                }
            }
        }

        Ok(constructs)
    }

    fn parse_conditional_construct(
        &self,
        start_idx: usize,
    ) -> Result<(ConditionalConstruct, usize)> {
        let mut blocks = Vec::new();
        let mut else_block = None;
        let mut i = start_idx;

        // First block must be #if
        let (if_directive, if_pos) = &self.directives[i];
        if if_directive.directive_type != DirectiveType::If {
            return Err(anyhow::anyhow!("Expected #if directive"));
        }

        let mut current_block_start = *if_pos;
        let mut current_condition = if_directive.condition.clone();
        let mut current_start_line = if_directive.line;

        i += 1;

        while i < self.directives.len() {
            let (directive, pos) = &self.directives[i];

            match directive.directive_type {
                DirectiveType::ElseIf => {
                    // End current block
                    blocks.push(ConditionalBlock {
                        condition: current_condition,
                        start_pos: current_block_start,
                        end_pos: *pos,
                        start_line: current_start_line,
                    });

                    // Start new block
                    current_block_start = *pos;
                    current_condition = directive.condition.clone();
                    current_start_line = directive.line;
                }
                DirectiveType::Else => {
                    // End current block
                    blocks.push(ConditionalBlock {
                        condition: current_condition,
                        start_pos: current_block_start,
                        end_pos: *pos,
                        start_line: current_start_line,
                    });

                    // Start else block
                    current_block_start = *pos;
                    current_condition = None;
                    current_start_line = directive.line;
                }
                DirectiveType::End => {
                    // End current block
                    if else_block.is_none() && current_condition.is_none() {
                        // This is an else block
                        else_block = Some(ConditionalBlock {
                            condition: None,
                            start_pos: current_block_start,
                            end_pos: *pos,
                            start_line: current_start_line,
                        });
                    } else {
                        // This is a regular conditional block
                        blocks.push(ConditionalBlock {
                            condition: current_condition,
                            start_pos: current_block_start,
                            end_pos: *pos,
                            start_line: current_start_line,
                        });
                    }

                    return Ok((ConditionalConstruct { blocks, else_block }, i + 1));
                }
                DirectiveType::If => {
                    return Err(anyhow::anyhow!(
                        "Unexpected nested #if at line {} (nesting not yet supported)",
                        directive.line
                    ));
                }
                DirectiveType::Script | DirectiveType::Test | DirectiveType::TestUtil | DirectiveType::Use => {
                    // Skip script/test/use directives in conditional parsing
                    // They're handled separately
                    continue;
                }
            }

            i += 1;
        }

        Err(anyhow::anyhow!(
            "Unclosed #if construct starting at line {}",
            if_directive.line
        ))
    }
}
