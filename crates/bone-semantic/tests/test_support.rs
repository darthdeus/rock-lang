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

/// Checks that every `//^ ty` marker in `src` names the type at its position.
pub fn check_types(src: &str) -> Result<(), String> {
    let file = EmbeddedFile {
        file: "<parse_rock>",
        package: None,
        src,
    };
    check_types_embedded(&[file], &[], &[])?;
    if test_utils::extract_annotations(src).is_empty() {
        return Err("no //^ annotations in source".to_string());
    }
    Ok(())
}

pub(crate) fn diagnostics(ws: &Workspace) -> Vec<&CompilerError> {
    ws.inferences_iter()
        .flat_map(|(_, inference)| inference.errors.values().flatten())
        .chain(
            ws.modules
                .iter()
                .flat_map(|module| module.lowering_errors.values().flatten()),
        )
        .chain(
            ws.modules
                .iter()
                .flat_map(|module| module.interface_errors.iter()),
        )
        .collect()
}

pub(crate) fn diagnostic_messages(ws: &Workspace) -> Vec<String> {
    diagnostics(ws)
        .into_iter()
        .map(|error| error.message.clone())
        .collect()
}

fn first_error(ws: &Workspace) -> Option<&CompilerError> {
    diagnostics(ws)
        .into_iter()
        .min_by_key(|e| e.location.offset)
}

pub fn stdlib_modules() -> Vec<skull_parser::Ast> {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            skull_parser::parse_module_clean(f.contents, ustr(&rock_stdlib::stdlib_file_id(f.name)))
                .unwrap()
        })
        .collect()
}

pub use test_utils::{EmbeddedFile, parse_embedded_files};

/// Checks `files` (after the trusted stdlib, whose `//^` comments are not
/// markers), with every package reachable under its own self name.
/// `cached_stdlib`, when not empty, holds `stdlib_sources` already parsed.
pub fn check_types_embedded(
    files: &[EmbeddedFile<'_>],
    stdlib_sources: &[(&str, &str)],
    cached_stdlib: &[skull_parser::Ast],
) -> Result<(), String> {
    let stdlib = stdlib_sources.iter().map(|&(file, src)| (file, None, src));
    let checked = files.iter().map(|f| (f.file, f.package, f.src));
    let all: Vec<EmbeddedFile<'_>> = stdlib
        .chain(checked)
        .map(|(file, package, src)| EmbeddedFile { file, package, src })
        .collect();
    let srcs: Vec<(&str, &str)> = all.iter().map(|f| (f.file, f.src)).collect();
    let mut modules = Vec::with_capacity(all.len());
    for (i, f) in all.iter().enumerate() {
        let mut module = match cached_stdlib.get(i) {
            Some(module) => {
                assert_eq!(module.file, ustr(f.file));
                module.clone()
            }
            None => parse_module(f.src, ustr(f.file)).map_err(|e| render_err(&srcs, &e))?,
        };
        // `parse_module` recovers past a malformed item; the compile gate still rejects it.
        if let Some(e) = module.recovery_errors.first() {
            return Err(render_err(&srcs, e));
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

    let ids: Vec<(u32, &str)> = all
        .iter()
        .filter_map(|f| f.package.map(|(name, id)| (id, name)))
        .collect();
    let mut aliases = bone_core::modules::NsAliases::default();
    let mut seen = std::collections::HashSet::new();
    for &(id, name) in &ids {
        if seen.insert(id) {
            aliases.packages.push(bone_core::modules::PackageAliases {
                id,
                self_name: ustr(name),
                aliases: ids
                    .iter()
                    .filter(|(other, _)| *other != id)
                    .map(|&(other, other_name)| (ustr(other_name), other))
                    .collect(),
            });
        }
    }

    let ws = bone_semantic::semantic_passes::semantic_pass_enforcing_identity_storage(
        &mut modules,
        &aliases,
    )
    .map_err(|e| render_err(&srcs, &e))?;
    if let Some(e) = first_error(&ws) {
        return Err(render_err(&srcs, e));
    }

    for f in files {
        let annotations = test_utils::extract_annotations(f.src);
        if annotations.is_empty() {
            continue;
        }
        let mid = ws.module_ids[&ustr(f.file)];
        let inf = ws
            .inference(mid)
            .ok_or_else(|| format!("no inference for {}", f.file))?;
        for ((start, end), expected) in &annotations {
            let marker = Span::new(ustr(f.file), (0, 0), (0, 0), (*start, *end));
            let actual = bone_semantic::query::type_at(&ws, mid, inf, *start).ok_or_else(|| {
                render_err(
                    &srcs,
                    &CompilerError::new_with_label(
                        marker,
                        format!("no type at marker (expected `{expected}`)"),
                    ),
                )
            })?;
            let actual_str = ws.render(actual.body, actual.ty);
            if actual_str != **expected {
                return Err(render_err(
                    &srcs,
                    &CompilerError::new_with_label(
                        actual.span,
                        format!("expected `{expected}`, found `{actual_str}`"),
                    ),
                ));
            }
        }
    }
    Ok(())
}
