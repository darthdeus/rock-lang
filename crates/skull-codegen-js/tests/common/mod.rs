//! Shared test helpers: build the stdlib workspace and emit an intrinsic's
//! scalar Rock-fallback body to JS.

use std::sync::Arc;

use skull_graph::instance_graph;
use skull_mir::instance::Instance;
use skull_mir::lower::{ScalarCtx, lower_instance_with_plans};
use skull_mir::mir::{FuncRef, MirFunc};
use skull_mir::monoty::MonoInterner;

/// The JS emitter's source, one string, for tests that pin its structure.
/// `session` comes last so each route reads its session seam after it.
pub const EMIT_SOURCE: &str = concat!(
    include_str!("../../src/emit/mod.rs"),
    include_str!("../../src/emit/lanes.rs"),
    include_str!("../../src/emit/function.rs"),
    include_str!("../../src/emit/structured.rs"),
    include_str!("../../src/emit/effects.rs"),
    include_str!("../../src/emit/inst.rs"),
    include_str!("../../src/emit/assemble.rs"),
    include_str!("../../src/emit/overlay.rs"),
    include_str!("../../src/emit/worklist.rs"),
    include_str!("../../src/emit/delivery.rs"),
    include_str!("../../src/emit/plan_stages.rs"),
    include_str!("../../src/emit/tables.rs"),
    include_str!("../../src/emit/globals.rs"),
    include_str!("../../src/emit/coroutine.rs"),
    include_str!("../../src/emit/analysis.rs"),
    include_str!("../../src/emit/session.rs")
);

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

/// A command for `name` with the same colour scrub as [`node`].
pub fn tool(name: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(name);
    cmd.env_remove("FORCE_COLOR").env("NO_COLOR", "1");
    cmd
}

/// Set to skip, instead of fail, the tests that run emitted JS when `node` is
/// not on PATH. CI has node, so a missing binary there must not read as a pass.
pub const SKIP_NODE_ENV: &str = "ROCK_SKIP_NODE_TESTS";

/// True when `node` runs. When it does not, panics unless [`SKIP_NODE_ENV`] is
/// set, in which case it logs the skip and returns false.
pub fn require_node() -> bool {
    if node()
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return true;
    }
    assert!(
        std::env::var_os(SKIP_NODE_ENV).is_some(),
        "`node` is not on PATH; install it or set {SKIP_NODE_ENV}=1 to skip the tests that run JS"
    );
    eprintln!("SKIP: `node` not on PATH ({SKIP_NODE_ENV} is set)");
    false
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

/// Reload the single function `func` over `cache` and return its overlay text.
pub fn overlay_of_fn(
    ws: &bone_core::workspace::Workspace,
    mid: bone_core::prelude::ModuleId,
    cache: &mut skull_codegen_js::emit::JsEmitCache,
    func: FuncRef,
) -> String {
    skull_codegen_js::emit::emit_overlay(
        ws,
        mid,
        cache,
        &[(func.module, bone_core::hir::BodyOwner::Function(func.func))],
    )
    .unwrap()
    .expect_overlay()
}

/// The program that prints `<root>()` after the given runtime pieces and defs.
fn print_root_program(runtime: &[&str], defs: &str, root: &str) -> String {
    format!("{}\n{defs}\nconsole.log({root}());", runtime.concat())
}

/// Define a local `eval_int` that runs `<root>()` under the listed
/// `skull_codegen_js` runtime pieces, in order. Each file names only the pieces
/// it needs, so a missing dependency between them still fails.
#[macro_export]
macro_rules! runtime_eval {
    (eval_int: $($piece:ident),+ $(,)?) => {
        fn eval_int(defs: &str, root: &str, expect: i64) {
            $crate::common::eval_int(&[$(skull_codegen_js::$piece),+], defs, root, expect);
        }
    };
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

/// The id of the `<test>` module.
pub fn test_mid(ws: &bone_core::workspace::Workspace) -> bone_core::prelude::ModuleId {
    *ws.module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module")
}

/// Emit the `<test>` module as a whole JS program.
pub fn emit_program_js(ws: &bone_core::workspace::Workspace) -> Result<String, String> {
    skull_codegen_js::emit::emit_program(ws, test_mid(ws))
        .map(|p| p.program)
        .map_err(|e| e.message)
}

/// The `<test>` module's fn named `name`.
pub fn test_fn_ref(ws: &bone_core::workspace::Workspace, name: &str) -> FuncRef {
    let mid = test_mid(ws);
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
pub fn emit_closure(ws: &bone_core::workspace::Workspace, root: &str) -> Result<String, String> {
    skull_codegen_js::emit::emit_with_closure(ws, &[(test_fn_ref(ws, root), root.to_string())])
        .map_err(|e| e.message)
}

/// Build the stdlib workspace (parse + semantic pass).
pub fn stdlib_ws() -> bone_core::workspace::Workspace {
    ws_with("fn main() {}")
}

/// Stdlib workspace plus `src` as the `<test>` module.
pub fn ws_with(src: &str) -> bone_core::workspace::Workspace {
    ws_with_named("<test>", src)
}

pub fn ws_with_named(file: &str, src: &str) -> bone_core::workspace::Workspace {
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
    ws: &bone_core::workspace::Workspace,
    mid: bone_core::prelude::ModuleId,
    fid: comfy_la_arena::Idx<bone_core::hir::Function>,
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
pub fn lower_test_fn(ws: &bone_core::workspace::Workspace, name: &str) -> skull_mir::mir::MirFunc {
    let mid = test_mid(ws);
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
pub fn emit_user(ws: &bone_core::workspace::Workspace, name: &str) -> String {
    let mf = lower_test_fn(ws, name);
    skull_codegen_js::emit::emit_fn(ws, &mf, name)
        .unwrap_or_else(|e| panic!("emit `{name}`: {}", e.message))
}

/// [`emit_user`] but returns the reject message instead of panicking (for
/// red-first observation and reject-shape pins).
pub fn emit_user_err(ws: &bone_core::workspace::Workspace, name: &str) -> String {
    let mf = lower_test_fn(ws, name);
    match skull_codegen_js::emit::emit_fn(ws, &mf, name) {
        Ok(js) => panic!("expected `{name}` to reject, got:\n{js}"),
        Err(e) => e.message,
    }
}
