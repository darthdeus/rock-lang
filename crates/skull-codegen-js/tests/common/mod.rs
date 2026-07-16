//! Shared test helpers: build the stdlib workspace and emit an intrinsic's
//! scalar Rock-fallback body to JS.

use skull_mir::lower::{lower_function, ScalarCtx};

/// Build the stdlib workspace (parse + semantic pass).
pub fn stdlib_ws() -> bone_semantic::inference::Workspace {
    let mut modules: Vec<skull_parser::Ast> = rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = ustr::Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect();
    modules.push(
        skull_parser::parse_module("fn main() {}", ustr::Ustr::from("<test>")).expect("parse"),
    );
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

/// Lower + emit the `@intrinsic` Rock-fallback overload of `name` with `arity`
/// params, under the JS name `<name>_fb`. Panics if none matches or emit fails.
/// The Vec overloads (multi-value return) reject, so the first that emits is the
/// scalar one.
pub fn emit_scalar(ws: &bone_semantic::inference::Workspace, name: &str, arity: usize) -> String {
    let scx = ScalarCtx::new(ws);
    for (_, &mid) in ws.module_ids.iter() {
        let module = ws.module(mid);
        for (fid, f) in module.functions.iter() {
            if f.name.as_str() != name
                || f.param_decls.len() != arity
                || f.lua_body.is_some()
                || !f.annotations.iter().any(|a| a.name.name.as_str() == "intrinsic")
            {
                continue;
            }
            let Ok(mf) = lower_function(ws, &scx, mid, fid) else { continue };
            if let Ok(js) = skull_codegen_js::emit::emit_fn(ws, &mf, &format!("{name}_fb")) {
                return js;
            }
        }
    }
    panic!("no emittable scalar `{name}`/{arity} overload");
}
