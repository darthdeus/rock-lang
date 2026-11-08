//! `@nomangle` discipline under JS emission.
//!
//! An `inline(js)` body is verbatim text -- the callee-discovery walk cannot
//! see the calls inside it, and plain-Rock co-emitted fns get mangled JS names
//! (`name__mX_fY`). The graph records `@nomangle` declarations named by these
//! opaque bodies, and the JS view publishes those roots under their bare names
//! (plain Rock lowered normally, inline(js) verbatim).
use crate::common::ws_with;
use rustc_hash::FxHashSet;
use std::sync::Arc;

fn function(ws: &bone_core::workspace::Workspace, name: &str) -> skull_mir::mir::FuncRef {
    let module = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let (func, _) = ws
        .module(module)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == name)
        .expect("function");
    skull_mir::mir::FuncRef { module, func }
}

fn emit_program_js(ws: &bone_core::workspace::Workspace) -> Result<String, String> {
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    skull_codegen_js::emit::emit_program(ws, mid)
        .map(|p| p.program)
        .map_err(|e| e.message)
}

#[test]
fn inline_js_body_roots_nomangle_rock_fn_under_bare_name() {
    let ws = ws_with(
        r#"
@nomangle fn web_boot_probe(x: int) -> int { return x + 7 }

inline(js) fn launch(x: int) -> int = """
  return web_boot_probe(x);
"""

fn main() {
    let a = launch(1)
    print("{a}")
}
"#,
    );
    let prog = emit_program_js(&ws).expect("emit js program");
    assert!(
        prog.contains("function web_boot_probe("),
        "@nomangle fn called from an inline(js) body must emit under its bare name:\n{prog}"
    );
}

#[test]
fn inline_js_body_roots_nomangle_inline_js_sibling() {
    let ws = ws_with(
        r#"
@nomangle inline(js) fn web_helper_probe(x: int) -> int = """
  return x * 5;
"""

inline(js) fn launch2(x: int) -> int = """
  return web_helper_probe(x);
"""

fn main() {
    let a = launch2(1)
    print("{a}")
}
"#,
    );
    let prog = emit_program_js(&ws).expect("emit js program");
    assert!(
        prog.contains("function web_helper_probe(") && prog.contains("return x * 5"),
        "@nomangle inline(js) fn referenced from another inline(js) body must emit verbatim under its bare name:\n{prog}"
    );
}

#[test]
fn artifact_view_records_inline_js_opaque_boundary_roots() {
    let ws = ws_with(
        r#"
@nomangle fn web_view_probe(x: int) -> int { return x + 7 }

inline(js) fn launch_view(x: int) -> int = """
  return web_view_probe(x);
"""

fn main() {
    let a = launch_view(1)
    print("{a}")
}
"#,
    );
    let main = function(&ws, "main");
    let launch = function(&ws, "launch_view");
    let target = function(&ws, "web_view_probe");
    let interner = Arc::new(skull_mir::monoty::MonoInterner::new());
    let roots = [main];
    let graph =
        skull_graph::instance_graph::build_in(&ws, roots.to_vec(), false, Arc::clone(&interner));
    let view = skull_graph::artifact::realize(
        &ws,
        &graph,
        &skull_graph::artifact::ArtifactRequest::new(
            skull_graph::target_projection::JAVASCRIPT_WEB,
            &roots,
        )
        .with_mappings(
            skull_graph::artifact::ArtifactMappings::for_backend(bone_core::hir::FnBackend::Js)
                .with_supply(&skull_codegen_js::JsBackendRealization),
        )
        .with_symbols(&skull_graph::artifact::REALIZED_SYMBOLS),
    );
    let targets = view
        .opaque_boundary_roots
        .get(&skull_graph::instance_graph::DeclKey(launch))
        .expect("the retained inline(js) owner must have an opaque root record");
    assert!(
        !targets.is_empty(),
        "opaque-boundary root population must be nonzero"
    );
    assert_eq!(targets, &FxHashSet::from_iter([target]));
}
