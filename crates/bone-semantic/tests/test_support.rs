//! Test-only helpers shared across bone's test modules.
//!
//! - `check_types(src)` runs the bone pipeline and asserts every `//^ ty`
//!   marker in the source resolves to the expected type via `query::type_at`.

// Both test harnesses include this module and each uses part of it.
#![allow(dead_code)]

use skull_parser::parse_module;

use bone_semantic::prelude::*;

pub fn render_err(sources: &[(&str, &str)], e: &CompilerError) -> String {
    let mut sf = SourceFiles::empty();
    for (path, content) in sources {
        sf.add_or_update_file(source_file_from_path_and_contents(
            path.to_string(),
            path.to_string(),
            content.to_string(),
        ));
    }
    e.render(&sf)
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

    let ws = bone_semantic::semantic_passes::semantic_pass_enforcing_identity_storage(
        &mut [module],
        &bone_core::modules::NsAliases::default(),
    )
    .map_err(|e| stage("semantic_pass", e))?;

    if let Some(e) = first_error(&ws) {
        return Err(stage("type-check", e.clone()));
    }

    // the single user module: everything in the workspace except the builtin one
    let mid = ws
        .source_modules()
        .next()
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

        let actual = bone_semantic::query::type_at(&ws, mid, inf, *start).ok_or_else(|| {
            stage(
                "type_at",
                CompilerError::new_with_label(
                    Span::new(Ustr::from("<parse_rock>"), (0, 0), (0, 0), (*start, *end)),
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

/// The first body, lowering or interface error in source order: a fixture
/// fails (and `@fail` passes) on it. `semantic_pass` Ok means "lowered and
/// checked", not "no errors": these are collected per module and body.
fn first_error(ws: &Workspace) -> Option<&CompilerError> {
    ws.inferences_iter()
        .flat_map(|(_, inf)| inf.errors.values().flatten())
        .chain(
            ws.modules
                .iter()
                .flat_map(|m| m.lowering_errors.values().flatten()),
        )
        .chain(ws.modules.iter().flat_map(|m| m.interface_errors.iter()))
        .min_by_key(|e| e.location.offset)
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

    let mk_span = |s: usize, e: usize| Span::new(module.file, (0, 0), (0, 0), (s, e));

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

/// One embedded source file in a multi-package fixture.
pub struct PackageFixture<'a> {
    pub file: &'a str,
    /// `None` creates a standalone package; `Some((self_name, package_id))` stamps a
    /// v1 package owner.
    pub package: Option<(&'a str, u32)>,
    pub src: &'a str,
}

/// Like [`check_types`] but over several files in possibly several packages:
/// each file joins the namespace tree, and every `//^ ty` marker in every file
/// must resolve. Cross-package alias
/// tables default to "every package is reachable under its own self name".
/// Trusted stdlib sources are prepended without treating their
/// internal `//^` comments as fixture assertions.
pub fn check_types_packages_with_stdlib(
    files: &[PackageFixture<'_>],
    stdlib_sources: &[(&str, &str)],
) -> Result<(), String> {
    check_types_packages_with_cached_stdlib(files, stdlib_sources, &[])
}

fn check_types_packages_with_cached_stdlib(
    files: &[PackageFixture<'_>],
    stdlib_sources: &[(&str, &str)],
    cached_stdlib: &[skull_parser::Ast],
) -> Result<(), String> {
    if !cached_stdlib.is_empty() {
        assert_eq!(cached_stdlib.len(), stdlib_sources.len());
        for (module, (file, _)) in cached_stdlib.iter().zip(stdlib_sources) {
            assert_eq!(module.file, ustr(file));
        }
    }
    let mut all = Vec::with_capacity(stdlib_sources.len() + files.len());
    all.extend(stdlib_sources.iter().map(|(file, src)| PackageFixture {
        file,
        package: None,
        src,
    }));
    all.extend(files.iter().map(|file| PackageFixture {
        file: file.file,
        package: file.package,
        src: file.src,
    }));
    check_types_packages_inner(&all, files, cached_stdlib)
}

fn check_types_packages_inner(
    all_files: &[PackageFixture<'_>],
    checked_files: &[PackageFixture<'_>],
    cached_stdlib: &[skull_parser::Ast],
) -> Result<(), String> {
    let mut srcs: Vec<(&str, &str)> = Vec::new();
    let mut modules = Vec::new();
    for (i, f) in all_files.iter().enumerate() {
        srcs.push((f.file, f.src));
        let mut module = if let Some(module) = cached_stdlib.get(i) {
            module.clone()
        } else {
            parse_module(f.src, ustr(f.file)).map_err(|e| render_err(&srcs, &e))?
        };
        if let Some(e) = module.recovery_errors.first() {
            return Err(render_err(&srcs, &e.clone()));
        }
        module.package = f
            .package
            .map(|(name, id)| rock_core::source_code::PackageTag {
                id,
                self_name: name.to_string(),
                identity_storage: Default::default(),
                root: String::new(),
            });
        modules.push(module);
    }

    // Default alias table: each package reachable under its own self name.
    let mut aliases = bone_core::modules::NsAliases::default();
    let ids: Vec<(u32, &str)> = all_files
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

    let ws = bone_semantic::semantic_passes::semantic_pass_enforcing_identity_storage(
        &mut modules,
        &aliases,
    )
    .map_err(|e| render_err(&srcs, &e))?;

    if let Some(e) = first_error(&ws) {
        return Err(render_err(&srcs, e));
    }

    // Check `//^` markers in every file.
    for f in checked_files.iter() {
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
                return Err("`?` dump markers are not supported in package fixtures".to_string());
            }
            let actual = bone_semantic::query::type_at(&ws, mid, inf, *start).ok_or_else(|| {
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

pub use test_utils::{EmbeddedFile, parse_embedded_files};

/// Check embedded modules with the compiler-owned stdlib export view
/// available as the v1 default prelude.
pub fn check_types_embedded_with_stdlib(
    files: &[EmbeddedFile<'_>],
    stdlib_sources: &[(&str, &str)],
) -> Result<(), String> {
    check_types_embedded_with_cached_stdlib(files, stdlib_sources, &[])
}

pub fn check_types_embedded_with_cached_stdlib(
    files: &[EmbeddedFile<'_>],
    stdlib_sources: &[(&str, &str)],
    cached_stdlib: &[skull_parser::Ast],
) -> Result<(), String> {
    let fixtures: Vec<PackageFixture<'_>> = files
        .iter()
        .map(|f| PackageFixture {
            file: f.file,
            package: f.package,
            src: f.src,
        })
        .collect();
    check_types_packages_with_cached_stdlib(&fixtures, stdlib_sources, cached_stdlib)
}
