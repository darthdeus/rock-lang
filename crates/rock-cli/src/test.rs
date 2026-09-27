//! `@test` discovery for the skull/bone test path's compile-failure fallback.
//! When `skull_emit_full` fails (a file that was *supposed* to fail to
//! compile), `skull_backend` re-parses the source here — front end only, via
//! `skull_parser` — to check whether every `@test` expected the failure
//! (`expect_failure` / `@ignore`). The happy path discovers tests from the
//! typechecked workspace (`skull::module_tests`); this fallback runs when a
//! compile failure means there is no typechecked workspace to read.

use crate::*;

/// A `@test` fn found by the front-end-only fallback parse. Only the two flags
/// the sole caller (`skull_backend`) keys off are kept — `output` / type-hole
/// metadata is verified on the typechecked path (`skull::SkullTest`), never
/// here.
#[derive(Debug, Clone)]
pub struct TestMetadata {
    pub ignored: bool,
    pub expect_failure: bool,
}

/// Parse each source file (front end only, via `skull_parser`) and collect its
/// `@test` fns with their `expect_failure` / `@ignore` flags. Files that don't
/// parse cleanly are skipped — they can't contribute a discoverable test, and
/// the caller only reaches here after a compile failure anyway.
pub(crate) fn find_test_functions_from_sources(sources: &SourceFiles) -> Vec<TestMetadata> {
    let mut test_functions = Vec::new();

    for file in sources.files.iter() {
        let (contents, file_label, dialect) = {
            let b = file.borrow();
            (b.contents.clone(), b.file.to_id_path(), b.dialect)
        };
        let ast = match skull_parser::parse_module_with_dialect(
            &contents,
            ustr::Ustr::from(file_label.as_str()),
            dialect,
        ) {
            Ok(ast) if ast.recovery_errors.is_empty() => ast,
            _ => continue,
        };
        for item in &ast.items {
            if !matches!(item.kind, skull_parser::ItemKind::Function(_)) {
                continue;
            }
            let Some(test_ann) = item
                .annotations
                .iter()
                .find(|a| a.name.name.as_str() == "test")
            else {
                continue;
            };
            let expect_failure = test_ann.args.iter().any(|arg| {
                arg.name.as_ref().map_or(false, |n| n.name.as_str() == "expect_failure")
                    && matches!(ast.expr_kind(arg.value), skull_parser::ExprKind::Bool(true))
            });
            let ignored = item
                .annotations
                .iter()
                .any(|a| a.name.name.as_str() == "ignore");
            test_functions.push(TestMetadata {
                ignored,
                expect_failure,
            });
        }
    }

    test_functions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discover(src: &str) -> Vec<TestMetadata> {
        let mut sf = SourceFiles::empty();
        sf.add_or_update_file(source_file_from_path_and_contents(
            "t.rock".to_string(),
            "t.rock".to_string(),
            src.to_string(),
        ));
        find_test_functions_from_sources(&sf)
    }

    // Pins the compile-failure fallback's contract: it discovers every `@test`
    // fn and reports the two flags its sole caller keys off — `expect_failure`
    // and `@ignore`. (Guards the skull-parser `@test` discovery path.)
    #[test]
    fn discovers_test_flags() {
        let src = "\
@test
fn plain() {}

@test(expect_failure = true)
fn fails() {}

@test
@ignore
fn skipped() {}

fn not_a_test() {}
";
        let tests = discover(src);
        assert_eq!(tests.len(), 3, "three @test fns discovered");
        assert_eq!(
            tests.iter().filter(|t| t.expect_failure).count(),
            1,
            "one @test(expect_failure = true)"
        );
        assert_eq!(
            tests.iter().filter(|t| t.ignored).count(),
            1,
            "one @test + @ignore"
        );
        assert_eq!(
            tests.iter().filter(|t| !t.expect_failure && !t.ignored).count(),
            1,
            "one plain @test"
        );
    }

    // A file that doesn't parse cleanly contributes no discoverable tests
    // (the fallback skips it, matching the old parser's skip-on-Err).
    #[test]
    fn unparseable_file_yields_nothing() {
        assert!(discover("fn broken( {").is_empty());
    }
}
