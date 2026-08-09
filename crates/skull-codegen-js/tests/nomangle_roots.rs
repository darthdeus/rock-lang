//! stone-web arc 2 MB: `@nomangle` discipline under JS emission.
//!
//! An `inline(js)` body is verbatim text -- the callee-discovery walk cannot
//! see the calls inside it, and plain-Rock co-emitted fns get mangled JS names
//! (`name__mX_fY`). So a `@nomangle` Rock fn called by original name from an
//! inline(js) body (the web boot seam: `stone_run_game` inline(js) variant ->
//! `web_run_game` / `web_run_frame`) was (a) never emitted and (b) would have
//! the wrong name if it were. Rule (mirrors the lua path's @nomangle
//! discipline): every emitted inline(js) body is scanned for identifiers
//! naming a workspace `@nomangle` fn; matches are rooted and emitted under
//! their BARE name (plain Rock lowered normally, inline(js) verbatim).
use crate::common::ws_with;


fn emit_program_js(ws: &bone_semantic::inference::Workspace) -> Result<String, String> {
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
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
        prog.contains("function web_boot_probe(") ,
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
