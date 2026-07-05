//! `@test` discovery for the skull/bone test path. The legacy rock test
//! *runner* (compile -> harness -> luajit) lived here too; it was removed with
//! the rock execution arm. What remains is the front-end-only parse that finds
//! `@test` functions + their `output` / `type_holes` / `expect_failure`
//! metadata, which `skull_backend` reuses.

use crate::*;

#[derive(Debug, Clone)]
pub struct TestMetadata {
    // Parsed for completeness; skull's current caller keys off `expect_failure`
    // / `ignored` only (the @test name is recovered at execution time).
    #[allow(dead_code)]
    pub function_name: String,
    pub expected_output: Option<String>,
    pub expected_type_holes: Option<Vec<(u32, String)>>,
    pub ignored: bool,
    pub expect_failure: bool,
}

impl TestMetadata {
    pub fn new(function_name: String) -> Self {
        Self {
            function_name,
            expected_output: None,
            expected_type_holes: None,
            ignored: false,
            expect_failure: false,
        }
    }

    pub fn with_expected_output(mut self, output: String) -> Self {
        self.expected_output = Some(output);
        self
    }

    pub fn with_expected_type_holes(mut self, holes: Vec<(u32, String)>) -> Self {
        self.expected_type_holes = Some(holes);
        self
    }

    pub fn with_ignored(mut self, ignored: bool) -> Self {
        self.ignored = ignored;
        self
    }

    pub fn with_expect_failure(mut self, expect_failure: bool) -> Self {
        self.expect_failure = expect_failure;
        self
    }
}

/// Parse each source file (front end only) and collect the `@test` functions
/// plus their annotation metadata. The compile pass upstream already validated
/// parsing, so failures here are by-construction unexpected; surface them rather
/// than silently skipping the file.
pub(crate) fn find_test_functions_from_sources(sources: &SourceFiles) -> Vec<TestMetadata> {
    let mut test_functions = Vec::new();

    for file in sources.files.iter() {
        let contents = file.borrow().contents.clone();
        let file_label = file.borrow().file.to_id_path();
        let tokens = match Tokenizer::from_source_file(&contents, file).tokenize() {
            Ok(tokens) => tokens,
            Err(e) => {
                eprintln!(
                    "rock test: skipping `{file_label}` for test discovery: tokenize error: {e}"
                );
                continue;
            }
        };

        let mut builder = AstBuilder::new_for_module(file.borrow().module_id);
        let mut parser = match Parser::new(tokens, &mut builder) {
            Ok(parser) => parser,
            Err(e) => {
                eprintln!(
                    "rock test: skipping `{file_label}` for test discovery: parser error: {e}"
                );
                continue;
            }
        };

        if let Ok(module) = parser.parse_module_from_source_file(file) {
            for item in &module.items {
                if let ItemKind::Function(func) = &item.kind {
                    // Check if function has @test annotation
                    if let Some(test_annotation) = item
                        .annotations
                        .iter()
                        .find(|annotation| annotation.name.name == "test")
                    {
                        let mut metadata = TestMetadata::new(func.name.name.to_string());

                        // Parse annotation parameters
                        if let Some(expected_output) =
                            parse_test_output_parameter(&test_annotation.args)
                        {
                            metadata = metadata.with_expected_output(expected_output);
                        }

                        if let Some(type_holes) =
                            parse_test_type_holes_parameter(&test_annotation.args)
                        {
                            metadata = metadata.with_expected_type_holes(type_holes);
                        }

                        if let Some(expect_failure) =
                            parse_test_expect_failure_parameter(&test_annotation.args)
                        {
                            metadata = metadata.with_expect_failure(expect_failure);
                        }

                        // Check if function has @ignore annotation
                        let is_ignored = item
                            .annotations
                            .iter()
                            .any(|annotation| annotation.name.name == "ignore");

                        if is_ignored {
                            metadata = metadata.with_ignored(true);
                        }

                        test_functions.push(metadata);
                    }
                }
            }
        }
    }

    test_functions
}

fn parse_test_output_parameter(args: &[AnnotationArg]) -> Option<String> {
    // Look for named argument: output = "string_literal"
    for arg in args {
        if let Some(name) = &arg.name {
            if name.name == "output" {
                let raw = match &arg.value {
                    AnnotationValue::String(content) => content.to_string(),
                    AnnotationValue::Expression(expr) => {
                        // `""` and any string-literal-without-interpolation
                        // parses as `InterpolatedString` (the parser routes
                        // every double-quoted literal through it). Recover
                        // the static contents when every part is a literal.
                        if let rock_types::expr::ExprKind::InterpolatedString(parts) = &expr.kind {
                            let mut buf = String::new();
                            for part in parts {
                                match part {
                                    rock_types::expr::InterpolatedPart::Literal(lit) => {
                                        buf.push_str(lit);
                                    }
                                    rock_types::expr::InterpolatedPart::Expr(_, _) => {
                                        return None;
                                    }
                                }
                            }
                            buf
                        } else {
                            return None;
                        }
                    }
                    _ => continue,
                };
                let processed = raw
                    .replace("\\n", "\n")
                    .replace("\\t", "\t")
                    .replace("\\r", "\r")
                    .replace("\\\\", "\\")
                    .replace("\\\"", "\"");
                return Some(processed);
            }
        }
    }
    None
}

fn parse_test_type_holes_parameter(args: &[AnnotationArg]) -> Option<Vec<(u32, String)>> {
    // Look for named argument: type_holes = [[0, "int"], [1, "string"], ...]
    for arg in args {
        if let Some(name) = &arg.name {
            if name.name == "type_holes" {
                if let AnnotationValue::Expression(expr) = &arg.value {
                    return parse_type_hole_array(expr);
                }
            }
        }
    }
    None
}

fn parse_test_expect_failure_parameter(args: &[AnnotationArg]) -> Option<bool> {
    // Look for named argument: expect_failure = true/false
    for arg in args {
        if let Some(name) = &arg.name {
            if name.name == "expect_failure" {
                if let AnnotationValue::Bool(value) = &arg.value {
                    return Some(*value);
                }
            }
        }
    }
    None
}

fn parse_type_hole_array(expr: &Expr) -> Option<Vec<(u32, String)>> {
    // Parse [[0, "int"], [1, "string"]] syntax
    match &expr.kind {
        ExprKind::ArrayLiteral(elements) => {
            let mut holes = Vec::new();

            for element in elements {
                if let ExprKind::ArrayLiteral(pair) = &element.kind {
                    if pair.len() == 2 {
                        // Parse [id, "type"] pair
                        if let (ExprKind::Int(id, _), ExprKind::String(type_str)) =
                            (&pair[0].kind, &pair[1].kind)
                        {
                            holes.push((*id as u32, type_str.to_string()));
                        }
                    }
                }
            }

            if !holes.is_empty() {
                Some(holes)
            } else {
                None
            }
        }
        _ => None,
    }
}
