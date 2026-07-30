//! Test-only helpers shared across bone's test modules.
//!
//! - `check_types(src)` runs the bone pipeline and asserts every `//^ ty`
//!   marker in the source resolves to the expected type via `query::type_at`.
//! - `ExpectPretty` renders any `Result<T, CompilerError>` failure through
//!   rock's ariadne pipeline before panicking.

use skull_parser::parse_module;

use crate::prelude::*;

pub fn render_err(sources: &[(&str, &str)], e: &CompilerError) -> String {
    let mut sf = SourceFiles { files: vec![] };
    for (path, content) in sources {
        sf.add_or_update_file(source_file_from_path_and_contents(
            path.to_string(),
            path.to_string(),
            content.to_string(),
        ));
    }
    e.render(&sf)
}

pub trait ExpectPretty<T> {
    fn expect_pretty(self, stage: &str, sources: &[(&str, &str)]) -> T;
}

impl<T> ExpectPretty<T> for Result<T, CompilerError> {
    fn expect_pretty(self, stage: &str, sources: &[(&str, &str)]) -> T {
        self.unwrap_or_else(|e| panic!("{stage} failed\n{}", render_err(sources, &e)))
    }
}

/// Parse `//^ ty` markers out of `src`, run the bone pipeline once,
/// and assert that each marked position has the expected type.
/// Rock treats `//` as a comment so the original source parses cleanly
/// with annotation lines inert.
pub fn check_types(src: &str) -> Result<(), String> {
    let srcs: &[(&str, &str)] = &[("<parse_rock>", src)];
    let stage = |stage: &str, e: CompilerError| format!("{stage} failed\n{}", render_err(srcs, &e));

    // Filename must match the `srcs` key above (and the fabricated span in the
    // no-marker path below) so ariadne can fetch the source for error rendering
    // -- a mismatch triggers a stderr "Unable to fetch source" warning per label.
    let module = parse_module(src, ustr("<parse_rock>")).map_err(|e| stage("parse_rock", e))?;

    // `parse_module` recovers past a malformed item and returns Ok, parking the
    // error on `recovery_errors` -- the compile gate rejects on it, so a fixture
    // must too. Without this a parse-rejected shape reads as ACCEPTED here.
    if let Some(e) = module.recovery_errors.first() {
        return Err(stage("parse_rock", e.clone()));
    }

    let ws = semantic_pass(&mut [module]).map_err(|e| stage("semantic_pass", e))?;

    // `semantic_pass` Ok means "lowered and checked", not "no type errors" —
    // body-level errors are collected per BodyOwner. A fixture section fails
    // (and `@fail` passes) on the first body error in source order. Lowering
    // errors (e.g. `_` placeholder inside an explicit lambda/block) live on the
    // module, not the inference, so include those too — a body that fails to
    // lower is a failure. Interface-position type errors (unknown / wrong-arity
    // type in a signature / global / field) recover to `Error` + a soft
    // `interface_errors` entry under total lowering, so a fixture naming a bad
    // type in a signature must fail here too, not slip through as `Ok`.
    let mut body_errs: Vec<&CompilerError> = ws
        .inferences_iter()
        .flat_map(|(_, inf)| inf.errors.values().flatten())
        .chain(ws.modules.iter().flat_map(|m| m.lowering_errors.values().flatten()))
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter()))
        .collect();
    body_errs.sort_by_key(|e| e.location.offset);
    if let Some(e) = body_errs.first() {
        return Err(stage("type-check", (*e).clone()));
    }

    // the single user module: everything in the workspace except the builtin one
    let mid = ws
        .modules
        .iter()
        .find(|m| m.id != ModuleId::builtin())
        .ok_or_else(|| "no user module after semantic_pass".to_string())?
        .id;

    let inf = ws
        .inference(mid)
        .ok_or_else(|| "no inference for user module".to_string())?;

    let annotations = test_utils::extract_annotations(src);
    if annotations.is_empty() {
        return Err("no //^ annotations in source".to_string());
    }

    for ((start, end), expected) in &annotations {
        if expected == "?" {
            return Err(stage("dump", dump_at(&ws, mid, inf, *start, *end)));
        }

        let actual = crate::query::type_at(&ws, mid, inf, *start).ok_or_else(|| {
            stage(
                "type_at",
                CompilerError::new_with_label(
                    Span {
                        file: Ustr::from("<parse_rock>"),
                        line_range: (0, 0),
                        col_range: (0, 0),
                        offset_range: (*start, *end),
                    },
                    format!("no type at marker (expected `{expected}`)"),
                ),
            )
        })?;

        let actual_str = ws.render(actual.body, actual.ty);
        if actual_str != **expected {
            return Err(stage(
                "type mismatch",
                CompilerError::new_with_label(
                    actual.span,
                    format!("expected `{expected}`, found `{actual_str}`"),
                ),
            ));
        }
    }
    Ok(())
}

/// Build a CompilerError that labels every interval in the position index
/// overlapping `[start, end)`. Used by the `//^ ?` debug marker.
fn dump_at(
    ws: &Workspace,
    mid: ModuleId,
    inf: &InferenceResult,
    start: usize,
    end: usize,
) -> CompilerError {
    let module = match ws.modules.get(mid.index()) {
        Some(m) => m,
        None => {
            return CompilerError::new_with_label(
                Span::unknown(),
                format!("dump: module {:?} not in workspace", mid),
            );
        }
    };

    let mk_span = |s: usize, e: usize| Span {
        file: module.file,
        line_range: (0, 0),
        col_range: (0, 0),
        offset_range: (s, e),
    };

    let mut labels: Vec<(usize, Span, String)> = Vec::new();

    for item in module.item_index().find(start, end) {
        let owner = item.val;
        let Some(body) = module.bodies.get(&owner) else {
            continue;
        };

        let bi = inf.per_fn.get(&owner);

        for interval in body.node_index().find(start, end) {
            let interval_span = mk_span(interval.start, interval.stop);

            let (span, label) = match interval.val {
                hir::NodeRef::StaticTy { mid, idx } => (
                    interval_span,
                    format!(
                        "Ty {:?} = {}",
                        interval.val,
                        ws.render(None, hir::TyId::Static(mid, idx))
                    ),
                ),
                hir::NodeRef::Expr(idx) => (
                    body.expr_spans.get(idx).copied().unwrap_or(interval_span),
                    format!(
                        "Expr {:?} = {:?}",
                        idx,
                        bi.and_then(|b| b.expr_types.get(idx))
                    ),
                ),

                hir::NodeRef::Statement(idx) => (
                    body.statement_spans
                        .get(idx)
                        .copied()
                        .unwrap_or(interval_span),
                    format!("Stmt {:?}", idx),
                ),

                hir::NodeRef::Binding(idx) => (
                    body.binding_spans
                        .get(idx)
                        .copied()
                        .unwrap_or(interval_span),
                    format!("Binding {:?} = {:?}", idx, body.binding_arena[idx].name),
                ),

                hir::NodeRef::LocalTy(idx) => (
                    body.lowered_ty_spans
                        .get(idx)
                        .copied()
                        .unwrap_or(interval_span),
                    match bi {
                        Some(b) => format!(
                            "LocalTy {:?} = {}",
                            idx,
                            ws.render(Some(b), hir::TyId::Local(idx))
                        ),
                        None => format!("LocalTy {:?} = <body not inferred>", idx),
                    },
                ),
            };

            labels.push((interval.start, span, label));
        }
    }

    labels.sort_by_key(|x| x.0);

    let mut err = CompilerError::new_with_label(
        mk_span(start, end),
        format!("dump @ [{start}, {end}) - {} interval(s)", labels.len(),),
    );

    for (_, span, label) in labels {
        err = err.with_label(span, label);
    }

    err
}
