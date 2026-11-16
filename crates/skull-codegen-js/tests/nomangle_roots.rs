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
    let prog = crate::common::emit_program_js(&ws).expect("emit js program");
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
  const helpers = { web_helper_probe };
  return helpers.web_helper_probe(x);
"""

fn main() {
    let a = launch2(1)
    print("{a}")
}
"#,
    );
    let prog = crate::common::emit_program_js(&ws).expect("emit js program");
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
    let main = crate::common::test_fn_ref(&ws, "main");
    let launch = crate::common::test_fn_ref(&ws, "launch_view");
    let target = crate::common::test_fn_ref(&ws, "web_view_probe");
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

#[test]
fn inline_js_properties_literals_and_substrings_do_not_root_functions() {
    let ws = ws_with(
        r#"
@nomangle inline(lua) fn scan_probe() -> int = "return 1"
inline(js) fn launch_scan() -> int = """
    const object = { scan_probe: 7, __json_record_failure: 1 };
    const $scan_probe = 9;
    // scan_probe()
    /* scan_probe() */
    const text = "scan_probe";
    const pattern = /scan_probe/;
    const template = `scan_probe`;
    return object /* gap */ . /* gap */ scan_probe + $scan_probe;
"""
fn main() { print(launch_scan()) }
"#,
    );
    let prog = crate::common::emit_program_js(&ws).expect("property names are not callees");
    assert!(!prog.contains("function scan_probe("));
    assert_eq!(
        crate::common::run_node(&format!(
            "{}\n{}\nmain();",
            skull_codegen_js::assemble_prelude(&prog, true, skull_codegen_js::JS_NODE_HOST, &[]),
            prog
        ))
        .trim(),
        "16"
    );
}

#[test]
fn template_expressions_root_nomangle_functions() {
    let ws = ws_with(
        r#"
@nomangle fn template_probe(x: int) -> int { return x + 7 }
inline(js) fn template_launch(x: int) -> string = """
    return `value ${template_probe(x)}`;
"""
fn main() { print(template_launch(1)) }
"#,
    );
    let prog = crate::common::emit_program_js(&ws).expect("template expressions retain callees");
    let runtime =
        skull_codegen_js::assemble_prelude(&prog, true, skull_codegen_js::JS_NODE_HOST, &[]);
    assert_eq!(
        crate::common::run_node(&format!("{runtime}\n{prog}\nmain();")).trim(),
        "value 8"
    );
}
