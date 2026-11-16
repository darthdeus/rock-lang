//! `@test(type_holes = [[id, "type"], ...])`: every `?` hole inside the test fn
//! must be listed with the type inference resolved for it. Ids are the
//! parser's, counted across the whole file in source order, so the check
//! reparses the file; a normal build never pays for it.

use bone_semantic::inference::Workspace;
use bone_semantic::prelude::{Idx, ModuleId};
use rock_core::{CompilerError, SourceFiles, Span};
use skull_parser::{
    Ast, Binding, ExprData, ExprKind, Item, ItemKind, Let, StatementKind, TypeExprKind,
};

/// The `@test` fns of `mid` (see [`skull::module_tests_with_ids`]), rejected
/// when a `type_holes` assertion does not hold.
pub(crate) fn module_tests(
    ws: &Workspace,
    mid: ModuleId,
    sources: &SourceFiles,
) -> Result<Vec<(Idx<bone_semantic::hir::Function>, skull::SkullTest)>, CompilerError> {
    let tests = skull::module_tests_with_ids(ws, mid)?;
    check(ws, mid, sources)?;
    Ok(tests)
}

fn check(ws: &Workspace, mid: ModuleId, sources: &SourceFiles) -> Result<(), CompilerError> {
    let module = &ws.modules[mid.index()];
    let asserts_holes = module.functions.iter().any(|(_, f)| {
        f.annotations.iter().any(|a| {
            a.is_builtin("test")
                && a.args.iter().any(|arg| {
                    arg.name
                        .as_ref()
                        .is_some_and(|n| n.name.as_str() == "type_holes")
                })
        })
    });
    if !asserts_holes {
        return Ok(());
    }
    let ast = reparse(module.file, sources)?;
    let mut holes = Vec::new();
    for (_, t) in ast.types.iter() {
        if let TypeExprKind::Hole(id, _) = t.kind {
            holes.push(Hole {
                id,
                at: t.span.offset_range().0,
                binding: None,
            });
        }
    }
    for (_, e) in ast.exprs.iter() {
        if let ExprKind::Hole(id, _) = e.kind {
            holes.push(Hole {
                id,
                at: e.span.offset_range().0,
                binding: None,
            });
        }
    }
    // A one-shot build keeps no offset index into bodies, so a hole is read
    // through the `let` binding it annotates.
    for (_, stmt) in ast.stmts.iter() {
        if let StatementKind::Let(Let {
            binding: Binding::Ident(name),
            ty: Some(ty),
            ..
        }) = &stmt.kind
            && let TypeExprKind::Hole(id, _) = ast.types[*ty].kind
            && let Some(hole) = holes.iter_mut().find(|hole| hole.id == id)
        {
            hole.binding = Some(name.span.offset_range().0);
        }
    }
    holes.sort_unstable_by_key(|hole| hole.id);
    let mut failures = Vec::new();
    check_items(ws, mid, &ast, &ast.items, &holes, &mut failures);
    match failures.first() {
        None => Ok(()),
        Some((span, _)) => {
            let message = failures
                .iter()
                .map(|(_, line)| line.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            Err(CompilerError::new_with_label(*span, message))
        }
    }
}

struct Hole {
    id: u32,
    at: usize,
    /// Where the name of the `let` this hole is the whole annotation of starts.
    binding: Option<usize>,
}

/// The module's source parsed again, as the build parsed it, so hole ids match.
fn reparse(file: ustr::Ustr, sources: &SourceFiles) -> Result<Ast, CompilerError> {
    let source = sources
        .files
        .iter()
        .find(|f| f.borrow().file.to_id_path() == file.as_str())
        .ok_or_else(|| {
            CompilerError::new_no_location(format!("type_holes: no source loaded for {file}"))
        })?;
    let source = source.borrow();
    skull_parser::parse_module_with_dialect(&source.contents, file, source.dialect)
}

fn check_items(
    ws: &Workspace,
    mid: ModuleId,
    ast: &Ast,
    items: &[Item],
    holes: &[Hole],
    failures: &mut Vec<(Span, String)>,
) {
    for item in items {
        let def = match &item.kind {
            ItemKind::Function(def) => def,
            ItemKind::Module(block) => {
                check_items(ws, mid, ast, &block.items, holes, failures);
                continue;
            }
            _ => continue,
        };
        let mut tests = item.annotations.iter().filter(|a| a.name.name == "test");
        let Some((anno, value)) = tests.find_map(|a| {
            let arg = a.args.iter().find(|arg| {
                arg.name
                    .as_ref()
                    .is_some_and(|n| n.name.as_str() == "type_holes")
            })?;
            Some((a, arg.value))
        }) else {
            continue;
        };
        let name = def.name.name;
        let problems = match expected_holes(ast, value) {
            Some(expected) => {
                let (start, end) = item.span.offset_range();
                let inside: Vec<&Hole> = holes
                    .iter()
                    .filter(|hole| start <= hole.at && hole.at < end)
                    .collect();
                compare(ws, mid, &expected, &inside)
            }
            None => vec![r#"type_holes expects [[id, "type"], ...]"#.to_string()],
        };
        if !problems.is_empty() {
            failures.push((
                anno.span,
                format!("type_holes on `{name}`: {}", problems.join("; ")),
            ));
        }
    }
}

/// `[[id, "type"], ...]`, or `None` when the literal has another shape.
fn expected_holes(ast: &Ast, list: Idx<ExprData>) -> Option<Vec<(u32, String)>> {
    let ExprKind::ArrayLiteral(pairs) = &ast.exprs[list].kind else {
        return None;
    };
    pairs
        .iter()
        .map(|&pair| match &ast.exprs[pair].kind {
            ExprKind::ArrayLiteral(parts) => match parts.as_slice() {
                [id, ty] => match (&ast.exprs[*id].kind, &ast.exprs[*ty].kind) {
                    (ExprKind::Int(id, _), ExprKind::String(ty)) => {
                        Some((u32::try_from(*id).ok()?, ty.to_string()))
                    }
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn compare(
    ws: &Workspace,
    mid: ModuleId,
    expected: &[(u32, String)],
    inside: &[&Hole],
) -> Vec<String> {
    let inferred = |hole: &Hole| {
        hole.binding
            .and_then(|at| let_type(ws, mid, at))
            .unwrap_or_else(|| "nothing".to_string())
    };
    let mut problems = Vec::new();
    for (i, (id, want)) in expected.iter().enumerate() {
        if expected[..i].iter().any(|(seen, _)| seen == id) {
            problems.push(format!("hole {id} is listed twice"));
            continue;
        }
        match inside.iter().find(|hole| hole.id == *id) {
            None => problems.push(format!("hole {id} is not in this test")),
            Some(hole) if hole.binding.is_none() => problems.push(format!(
                "hole {id} is not a whole `let` annotation, which is all type_holes checks"
            )),
            Some(hole) => {
                let got = inferred(hole);
                if &got != want {
                    problems.push(format!("hole {id}: expected `{want}`, inferred `{got}`"));
                }
            }
        }
    }
    for hole in inside {
        if !expected.iter().any(|(listed, _)| *listed == hole.id) {
            problems.push(format!(
                "hole {} (`{}`) is not listed",
                hole.id,
                inferred(hole)
            ));
        }
    }
    problems
}

/// The declared type of the `let` binding whose name starts at `at`, resolved
/// by its body's inference.
fn let_type(ws: &Workspace, mid: ModuleId, at: usize) -> Option<String> {
    let module = &ws.modules[mid.index()];
    let owner = module.body_at(at)?;
    let body = module.bodies.get(&owner)?;
    let inference = ws.inference(mid)?.per_fn.get(&owner)?;
    let ty = body
        .binding_arena
        .iter()
        .find(|(_, b)| b.span.offset_range().0 == at)?
        .1
        .declared_ty?;
    Some(ws.render(Some(inference), ty))
}

#[cfg(test)]
mod tests {
    fn check(src: &str) -> Result<(), String> {
        let file = ustr::ustr("holes.rock");
        let ast = skull_parser::parse_module(src, file).expect("parse");
        let ws = crate::bone_backend::check_with_stdlib([ast]).expect("type-check");
        let mid = crate::bone_backend::entry_module_id(&ws, file);
        let sources = crate::common::single_source("holes.rock", src);
        super::module_tests(&ws, mid, &sources)
            .map(drop)
            .map_err(|e| e.message)
    }

    const TWO_TESTS: &str = r#"
@test(type_holes = [[0, "int"], [1, "[string]"]])
fn first() {
    let x: ? = 42
    let names: ? = ["a"]
    assert_eq(x, 42)
    assert_eq(names[0], "a")
}

@test(type_holes = [[2, "float"]])
fn second() {
    let y: ? = 1.5
    assert_eq(y, 1.5)
}
"#;

    #[test]
    fn listed_holes_with_their_inferred_types_pass() {
        assert_eq!(check(TWO_TESTS), Ok(()));
    }

    #[test]
    fn a_wrong_expected_type_fails() {
        let src = TWO_TESTS.replace(r#"[1, "[string]"]"#, r#"[1, "[int]"]"#);
        let err = check(&src).expect_err("wrong type must fail");
        assert!(
            err.contains("`first`")
                && err.contains("hole 1: expected `[int]`, inferred `[string]`"),
            "{err}"
        );
    }

    #[test]
    fn a_listed_id_outside_the_test_fails() {
        let src = TWO_TESTS.replace(r#"[[2, "float"]]"#, r#"[[2, "float"], [0, "int"]]"#);
        let err = check(&src).expect_err("missing id must fail");
        assert!(
            err.contains("`second`") && err.contains("hole 0 is not in this test"),
            "{err}"
        );
    }

    #[test]
    fn an_unlisted_hole_fails() {
        let src = TWO_TESTS.replace(r#", [1, "[string]"]"#, "");
        let err = check(&src).expect_err("extra hole must fail");
        assert!(
            err.contains("`first`") && err.contains("hole 1 (`[string]`) is not listed"),
            "{err}"
        );
    }

    #[test]
    fn the_shared_test_front_end_checks_holes() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("holes.rock");
        let src = TWO_TESTS
            .replace(r#"[2, "float"]"#, r#"[2, "int"]"#)
            .replace("fn second", "fn a_test_with_a_long_descriptive_name");
        std::fs::write(&path, src).expect("write test file");
        let Err(failure) = crate::test_unit::check_file(path.to_str().unwrap(), false) else {
            panic!("hole mismatch must reject the unit");
        };
        assert!(
            failure
                .reason
                .contains("hole 2: expected `int`, inferred `float`"),
            "{}",
            failure.reason
        );
    }

    #[test]
    fn a_malformed_list_fails() {
        let src = TWO_TESTS.replace(r#"[[2, "float"]]"#, r#"[2, "float"]"#);
        let err = check(&src).expect_err("malformed list must fail");
        assert!(err.contains("type_holes expects"), "{err}");
    }
}
