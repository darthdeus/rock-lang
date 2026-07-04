//! Step 2: lower bone HIR -> MIR. We start with the simplest possible body — an
//! int literal returned from a function — so we wire in bone (parse +
//! semantic_pass + the build entry) BEFORE any SSA subtlety. No `let`/`Var` yet
//! (2b), no operators (2c), no control flow (step 3).
//!
//! `fn f() -> int { 5 }` should lower to:
//!
//! ```text
//! fn f() {
//!   bb0:
//!     v0 = const 5
//!     return v0
//! }
//! ```
//!
//! What this pins:
//!   - `bone_mir::build::lower_function(&Workspace, ModuleId, Idx<Function>)
//!      -> Result<MirFunc, CompilerError>` — the build entry over the
//!      (func, body, inf) triple.
//!   - the body's tail expression flows out as the function's return value
//!     (`{ 5 }` has no explicit `return`; the tail is the result).

use bone_semantic::prelude::{ModuleId, Ustr};
use bone_semantic::semantic_passes::semantic_pass;

/// Parse + type-check `src`, lower the function named `f`, return its MIR dump.
fn lower_f(src: &str) -> String {
    let module = skull_parser::parse_module(src, Ustr::from("<build-test>")).expect("parse");
    let ws = semantic_pass(&mut [module]).expect("semantic_pass");

    let mid = ws
        .modules
        .iter()
        .map(|m| m.id)
        .find(|m| *m != ModuleId::builtin())
        .expect("user module");
    let fid = ws.module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "f")
        .map(|(idx, _)| idx)
        .expect("fn f");

    bone_mir::build::lower_function(&ws, mid, fid)
        .expect("lower")
        .to_string()
}

#[test]
fn lower_int_literal() {
    let got = lower_f("fn f() -> int { 5 }");
    assert_eq!(
        got,
        "\
fn f() {
  bb0:
    v0 = const 5
    return v0
}"
    );
}

#[test]
fn lower_let_binding() {
    // `let a = 5` declares a cell + stores into it; `a` reads it back.
    let got = lower_f("fn f() -> int { let a = 5; a }");
    assert_eq!(
        got,
        "\
fn f() {
  bb0:
    v0 = const 5
    store c0, v0
    v1 = load c0
    return v1
}"
    );
}
