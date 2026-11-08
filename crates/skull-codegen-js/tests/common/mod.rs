//! Shared test helpers: build the stdlib workspace and emit an intrinsic's
//! scalar Rock-fallback body to JS.

use std::sync::Arc;

use skull_graph::instance_graph;
use skull_mir::instance::Instance;
use skull_mir::lower::{ScalarCtx, lower_instance_with_plans};
use skull_mir::mir::{FuncRef, MirFunc};
use skull_mir::monoty::MonoInterner;

pub struct InlineOffGuard(#[allow(dead_code)] skull_transform::InlineOffGuard);

impl InlineOffGuard {
    pub fn on() -> Self {
        InlineOffGuard(skull_transform::inline_off_guard())
    }
}

/// Every `node` spawn in this crate's tests goes through here.
///
/// `console.log` of a NUMBER is formatted by `util.inspect`, which colours it,
/// and `FORCE_COLOR` forces that on even when stdout is a pipe. A harness that
/// parses stdout then receives `"\x1b[33m9901\x1b[39m"` and fails to read an
/// int -- the value is right, the bytes are not.
///
/// **`NO_COLOR` alone does not work**: node ignores it whenever `FORCE_COLOR`
/// is set, and says so on stderr. The variable has to be REMOVED, so a fix that
/// only sets `NO_COLOR` reads correctly and does nothing.
pub fn node() -> std::process::Command {
    tool("node")
}

/// Same scrub, for a tool named by a variable rather than a literal.
/// `nb_differential` and `number_format` run one fixture through BOTH `node`
/// and `luajit` and compare the printed tokens, so they reach a spawn this way
/// and a literal-matching sweep misses them.
pub fn tool(name: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(name);
    cmd.env_remove("FORCE_COLOR").env("NO_COLOR", "1");
    cmd
}

/// Set to skip, instead of fail, the tests that run emitted JS when `node` is
/// not on PATH. CI has node, so a missing binary there must not read as a pass.
pub const SKIP_NODE_ENV: &str = "ROCK_SKIP_NODE_TESTS";

fn tool_runs(name: &str, version_arg: &str) -> bool {
    tool(name)
        .arg(version_arg)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// True when `node` runs. When it does not, panics unless [`SKIP_NODE_ENV`] is
/// set, in which case it logs the skip and returns false.
pub fn require_node() -> bool {
    if tool_runs("node", "--version") {
        return true;
    }
    assert!(
        std::env::var_os(SKIP_NODE_ENV).is_some(),
        "`node` is not on PATH; install it or set {SKIP_NODE_ENV}=1 to skip the tests that run JS"
    );
    eprintln!("SKIP: `node` not on PATH ({SKIP_NODE_ENV} is set)");
    false
}

/// [`require_node`] for the differentials that also run `luajit`, which the
/// rest of the workspace's tests require unconditionally.
pub fn require_node_and_luajit() -> bool {
    if !require_node() {
        return false;
    }
    assert!(tool_runs("luajit", "-v"), "`luajit` is not on PATH");
    true
}

/// Run `program` under `node -e` and return its stdout. Panics, showing the
/// program and stderr, when node exits non-zero.
pub fn run_node(program: &str) -> String {
    let out = node().arg("-e").arg(program).output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The program that prints `<root>()` after the given runtime pieces and defs.
fn print_root_program(runtime: &[&str], defs: &str, root: &str) -> String {
    format!("{}\n{defs}\nconsole.log({root}());", runtime.concat())
}

/// Evaluate `<root>()` under node and check it prints the integer `expect`.
pub fn eval_int(runtime: &[&str], defs: &str, root: &str, expect: i64) {
    if !require_node() {
        return;
    }
    let program = print_root_program(runtime, defs, root);
    let s = run_node(&program);
    let got: i64 = s
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("not an int: {s:?}\n{program}"));
    assert_eq!(got, expect, "wrong result for `{root}`\n{program}");
}

/// Evaluate `<root>()` under node and check it prints exactly `expect`.
pub fn eval_str(runtime: &[&str], defs: &str, root: &str, expect: &str) {
    if !require_node() {
        return;
    }
    let program = print_root_program(runtime, defs, root);
    let s = run_node(&program);
    assert_eq!(
        s.trim_end_matches('\n'),
        expect,
        "wrong result for `{root}`\n{program}"
    );
}

/// The `<test>` module's fn named `name`.
pub fn test_fn_ref(ws: &bone_semantic::inference::Workspace, name: &str) -> FuncRef {
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == name {
            return FuncRef {
                module: mid,
                func: fid,
            };
        }
    }
    panic!("no fn `{name}` in <test> module");
}

/// Emit the `<test>` fn `root` and its callee closure to JS.
pub fn emit_closure(
    ws: &bone_semantic::inference::Workspace,
    root: &str,
) -> Result<String, String> {
    skull_codegen_js::emit::emit_with_closure(ws, &[(test_fn_ref(ws, root), root.to_string())])
        .map_err(|e| e.message)
}

/// Build the stdlib workspace (parse + semantic pass).
pub fn stdlib_ws() -> bone_semantic::inference::Workspace {
    ws_with("fn main() {}")
}

/// Stdlib workspace plus `src` as the `<test>` module.
pub fn ws_with(src: &str) -> bone_semantic::inference::Workspace {
    ws_with_named("<test>", src)
}

pub fn ws_with_named(file: &str, src: &str) -> bone_semantic::inference::Workspace {
    let mut modules: Vec<skull_parser::Ast> = rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = ustr::Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module_clean(f.contents, id).expect("stdlib parse")
        })
        .collect();
    modules.push(skull_parser::parse_module_clean(src, ustr::Ustr::from(file)).expect("parse"));
    bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass")
}

/// Lower one test root through the graph-owned concrete plan. Keeping this
/// helper here makes every JS MIR fixture exercise the same entry as the
/// production emitter, including the callee table and shared type arena.
pub fn lower_test_ref(
    ws: &bone_semantic::inference::Workspace,
    mid: bone_semantic::prelude::ModuleId,
    fid: comfy_la_arena::Idx<bone_semantic::hir::Function>,
) -> MirFunc {
    let root = FuncRef {
        module: mid,
        func: fid,
    };
    let interner = Arc::new(MonoInterner::new());
    let graph = instance_graph::build_in(ws, vec![root], false, Arc::clone(&interner));
    let scx = ScalarCtx::new(ws).sharing(interner);
    let concrete = skull_graph::concrete_body::build(ws, &graph);
    let plan = concrete
        .plans
        .get(&Instance::base(FuncRef {
            module: mid,
            func: fid,
        }))
        .unwrap_or_else(|| panic!("no concrete plan for <test> function {fid:?}"));
    lower_instance_with_plans(ws, &scx, plan, &concrete.plans)
        .unwrap_or_else(|e| panic!("lower concrete test root: {}", e.message))
}

/// Lower the fn named `name` in the `<test>` module (must be unique there).
pub fn lower_test_fn(
    ws: &bone_semantic::inference::Workspace,
    name: &str,
) -> skull_mir::mir::MirFunc {
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    for (fid, f) in ws.module(mid).functions.iter() {
        if f.name.as_str() == name {
            return lower_test_ref(ws, mid, fid);
        }
    }
    panic!("no fn `{name}` in <test> module");
}

/// Lower the fn named `name` in the `<test>` module and emit it to JS under its
/// own name. Panics if the fn is missing or emit rejects (the reject message is
/// surfaced so a red test records it).
pub fn emit_user(ws: &bone_semantic::inference::Workspace, name: &str) -> String {
    let mf = lower_test_fn(ws, name);
    skull_codegen_js::emit::emit_fn(ws, &mf, name)
        .unwrap_or_else(|e| panic!("emit `{name}`: {}", e.message))
}

/// [`emit_user`] but returns the reject message instead of panicking (for
/// red-first observation and reject-shape pins).
pub fn emit_user_err(ws: &bone_semantic::inference::Workspace, name: &str) -> String {
    let mf = lower_test_fn(ws, name);
    match skull_codegen_js::emit::emit_fn(ws, &mf, name) {
        Ok(js) => panic!("expected `{name}` to reject, got:\n{js}"),
        Err(e) => e.message,
    }
}

/// Lower + emit the `@intrinsic` Rock-fallback overload of `name` with `arity`
/// params, under the JS name `<name>_fb`. Panics if none matches or emit fails.
/// The Vec overloads (multi-value return) reject, so the first that emits is the
/// scalar one.
pub fn emit_scalar(ws: &bone_semantic::inference::Workspace, name: &str, arity: usize) -> String {
    for &mid in ws.module_ids.values() {
        let module = ws.module(mid);
        for (fid, f) in module.functions.iter() {
            if f.name.as_str() != name
                || f.param_decls.len() != arity
                || f.inline_body.is_some()
                || !f
                    .annotations
                    .iter()
                    .any(|a| a.name.name.as_str() == "intrinsic")
            {
                continue;
            }
            let mf = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                lower_test_ref(ws, mid, fid)
            }));
            let Ok(mf) = mf else { continue };
            if let Ok(js) = skull_codegen_js::emit::emit_fn(ws, &mf, &format!("{name}_fb")) {
                return js;
            }
        }
    }
    panic!("no emittable scalar `{name}`/{arity} overload");
}
