//! Finding 2: `ScopeTree::build`
//! panicked ("no entry found for key") when `fold_const_branches` removed the
//! block holding an OUTER lambda's creation while the outer's body blocks
//! (containing an INNER lambda) lingered in the arena -- the inner lambda's
//! creating block belonged to no live scope, so the parent lookup missed. The
//! whole file's compile died on legal code.
//!
//! Fix: scope analysis considers only lambdas whose creation site is REACHABLE
//! (a fixpoint from the fn entry through live scopes); a transitively-dead
//! lambda gets no scope. These compile-and-run without panicking.
use crate::common::{compile_and_run, emit_module, mangled_name, run_main};

/// The confirmed repro: an outer lambda inside `if false`, itself holding an
/// inner lambda. Both are dead; the live `return 42` path must compile.
#[test]
fn dead_nested_lambda_in_const_false_branch_compiles() {
    let src = r#"
fn thing() -> int {
    if false {
        let outer = () => {
            let inner = () => 3
            return inner()
        }
        return outer()
    }
    return 42
}
fn main() { print("{thing()}") }
"#;
    let out = compile_and_run(src);
    assert_eq!(
        out, "42",
        "the live branch runs; the dead nested lambdas are dropped; got: {out}"
    );
}

/// A 3-deep nest inside a const-false branch: outer -> mid -> inner, all dead.
#[test]
fn dead_three_deep_nested_lambda_compiles() {
    let src = r#"
fn thing() -> int {
    if false {
        let outer = () => {
            let mid = () => {
                let inner = () => 7
                return inner()
            }
            return mid()
        }
        return outer()
    }
    return 5
}
fn main() { print("{thing()}") }
"#;
    let out = compile_and_run(src);
    assert_eq!(
        out, "5",
        "the live branch runs; three dead nested lambdas are dropped; got: {out}"
    );
}

/// A const-false branch whose inner lambda CAPTURES an outer local: the dead
/// subtree references a captured cell but must still be dropped cleanly (no scope
/// for a dead lambda means no capture analysis over it).
#[test]
fn dead_nested_lambda_capturing_outer_local_compiles() {
    let src = r#"
fn thing(x: int) -> int {
    if false {
        let outer = () => {
            let inner = () => x + 1
            return inner()
        }
        return outer()
    }
    return x * 2
}
fn main() { print("{thing(4)}") }
"#;
    let out = compile_and_run(src);
    assert_eq!(
        out, "8",
        "the live branch runs; the dead capturing lambdas are dropped; got: {out}"
    );
}

/// Control: REACHABLE nested lambdas still hoist and run (the fix must not drop
/// live lambdas). outer()() reads x threaded through both envs.
#[test]
fn reachable_nested_lambdas_still_hoist() {
    let src = r#"
fn thing(x: int) -> int {
    if x > 0 {
        let outer = () => {
            let inner = () => x + 1
            return inner()
        }
        return outer()
    }
    return 0
}
fn main() { print("{thing(4)}") }
"#;
    let lua = emit_module(src);
    assert!(
        lua.contains(&format!("function {}__lambda", mangled_name(&lua, "thing"))),
        "reachable nested lambdas must still hoist:\n{lua}"
    );
    let out = run_main(&lua);
    assert_eq!(
        out, "5",
        "the reachable nested capture reads x (4+1); got: {out}"
    );
}
