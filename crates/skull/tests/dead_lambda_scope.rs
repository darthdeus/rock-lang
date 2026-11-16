//! Finding 2: `ScopeTree::build`
//! panicked ("no entry found for key") when `fold_const_branches` removed the
//! block holding an OUTER lambda's creation while the outer's body blocks
//! (containing an INNER lambda) lingered in the arena -- the inner lambda's
//! creating block belonged to no live scope, so the parent lookup missed. The
//! whole file's compile died on legal code.
//!
//! Fix: scope analysis considers only lambdas whose creation site is REACHABLE
//! (a fixpoint from the fn entry through live scopes); a transitively-dead
//! lambda gets no scope. The dead cases run from
//! tests/closures/dead_nested_lambdas.rock.
use crate::common::{emit_module, mangled_name, run_main};

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
