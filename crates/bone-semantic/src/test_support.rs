//! Test-only helpers shared across bone's test modules.
//!
//! - `check_types(src)` runs the bone pipeline and asserts every `//^ ty`
//!   marker in the source resolves to the expected type via `query::type_at`.
//! - `ExpectPretty` renders any `Result<T, CompilerError>` failure through
//!   rock's ariadne pipeline before panicking.

use skull_parser::{parse_module, parse_module_with_dialect};

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

/// One embedded source file in a multi-package fixture (plan M2).
pub struct PackageFixture<'a> {
    pub file: &'a str,
    /// `None` = legacy standalone; `Some((self_name, package_id))` stamps a
    /// v1 package owner.
    pub package: Option<(&'a str, u32)>,
    pub dialect: skull_parser::Dialect,
    pub src: &'a str,
}

/// Like [`check_types`] but over several files in possibly several packages:
/// each file parses under its dialect, v1 files join the namespace tree, and
/// every `//^ ty` marker in every file must resolve. Cross-package alias
/// tables default to "every package is reachable under its own self name".
pub fn check_types_packages(files: &[PackageFixture<'_>]) -> Result<(), String> {
    let mut srcs: Vec<(&str, &str)> = Vec::new();
    let mut modules = Vec::new();
    for f in files {
        srcs.push((f.file, f.src));
        let mut module = parse_module_with_dialect(f.src, ustr(f.file), f.dialect)
            .map_err(|e| render_err(&srcs, &e))?;
        if let Some(e) = module.recovery_errors.first() {
            return Err(render_err(&srcs, &e.clone()));
        }
        module.package = f.package.map(|(name, id)| {
            rock_core::source_code::PackageTag {
                id,
                self_name: name.to_string(),
            }
        });
        modules.push(module);
    }

    // Default alias table: each package reachable under its own self name.
    let mut aliases = bone_core::modules::NsAliases::default();
    let ids: Vec<(u32, &str)> = files
        .iter()
        .filter_map(|f| f.package.map(|(name, id)| (id, name)))
        .collect();
    let mut seen = std::collections::HashSet::new();
    for (id, name) in &ids {
        if !seen.insert(*id) {
            continue;
        }
        let self_u = ustr(name);
        let entry = bone_core::modules::PackageAliases {
            id: *id,
            self_name: self_u,
            aliases: ids
                .iter()
                .filter(|(other, _)| other != id)
                .map(|(other, other_name)| (ustr(other_name), *other))
                .collect(),
        };
        aliases.packages.push(entry);
    }

    let ws = semantic_pass_with_aliases(&mut modules, &aliases)
        .map_err(|e| render_err(&srcs, &e))?;

    // Body / lowering / interface errors fail the fixture (an `@fail` caller
    // treats any Err as the expected outcome).
    let mut body_errs: Vec<&CompilerError> = ws
        .inferences_iter()
        .flat_map(|(_, inf)| inf.errors.values().flatten())
        .chain(ws.modules.iter().flat_map(|m| m.lowering_errors.values().flatten()))
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter()))
        .collect();
    body_errs.sort_by_key(|e| e.location.offset);
    if let Some(e) = body_errs.first() {
        return Err(render_err(&srcs, *e));
    }

    // Check `//^` markers in every file.
    for f in files.iter() {
        let annotations = test_utils::extract_annotations(f.src);
        if annotations.is_empty() {
            continue;
        }
        let mid = ws
            .modules
            .iter()
            .find(|m| m.file == ustr(f.file))
            .ok_or_else(|| format!("file {} missing from workspace", f.file))?
            .id;
        let inf = ws
            .inference(mid)
            .ok_or_else(|| format!("no inference for {}", f.file))?;
        for ((start, _end), expected) in &annotations {
            if expected == "?" {
                return Err(format!("`?` dump markers are not supported in package fixtures"));
            }
            let actual = crate::query::type_at(&ws, mid, inf, *start).ok_or_else(|| {
                format!("no type at marker (expected `{expected}`) in {}", f.file)
            })?;
            let actual_str = ws.render(actual.body, actual.ty);
            if actual_str != **expected {
                return Err(format!(
                    "type mismatch in {}: expected `{expected}`, found `{actual_str}`",
                    f.file
                ));
            }
        }
    }
    Ok(())
}

/// One embedded file inside a fixture section: `//-- <file> @pkg=<name>,<id> @v1`.
pub struct EmbeddedFile<'a> {
    pub file: &'a str,
    pub package: Option<(&'a str, u32)>,
    pub dialect: skull_parser::Dialect,
    pub src: &'a str,
}

/// Split a fixture section body into embedded files. Lines `//-- <name>
/// [@pkg=<self_name>,<id>] [@v1]` start a file; everything until the next
/// marker is that file's source.
pub fn parse_embedded_files(body: &str) -> Vec<EmbeddedFile<'_>> {
    let mut files: Vec<EmbeddedFile> = Vec::new();
    let mut current: Option<(&str, Option<(&str, u32)>, skull_parser::Dialect, usize)> = None;
    for (idx, line) in body.lines().enumerate() {
        if let Some(rest) = line.trim_start().strip_prefix("//--") {
            if let Some(prev) = current.take() {
                let src = body.lines().skip(prev.3 + 1).take(idx - prev.3 - 1).collect::<Vec<_>>().join("\n");
                files.push(EmbeddedFile {
                    file: prev.0,
                    package: prev.1,
                    dialect: prev.2,
                    src: Box::leak(src.into_boxed_str()),
                });
            }
            let mut it = rest.trim().split_whitespace();
            let file = it.next().unwrap_or("file.rock");
            let mut package = None;
            let mut dialect = skull_parser::Dialect::Legacy;
            for token in it {
                if let Some(spec) = token.strip_prefix("@pkg=") {
                    let mut parts = spec.split(',');
                    let name: &str = parts.next().unwrap_or("pkg");
                    let id = parts.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
                    package = Some((leaked, id));
                } else if token == "@v1" {
                    dialect = skull_parser::Dialect::V1;
                }
            }
            let leaked_file: &'static str = Box::leak(file.to_string().into_boxed_str());
            current = Some((leaked_file, package, dialect, idx));
        }
    }
    if let Some(prev) = current.take() {
        let src = body.lines().skip(prev.3 + 1).collect::<Vec<_>>().join("\n");
        files.push(EmbeddedFile {
            file: prev.0,
            package: prev.1,
            dialect: prev.2,
            src: Box::leak(src.into_boxed_str()),
        });
    }
    files
}

/// Check one multi-file fixture section: parse the embedded files, run the
/// pipeline with default (self-name) alias tables, and verify `//^` markers.
pub fn check_types_embedded(files: &[EmbeddedFile<'_>]) -> Result<(), String> {
    let fixtures: Vec<PackageFixture<'_>> = files
        .iter()
        .map(|f| PackageFixture {
            file: f.file,
            package: f.package,
            dialect: f.dialect,
            src: f.src,
        })
        .collect();
    check_types_packages(&fixtures)
}
