//! A USER fn whose body fails to emit must reject with a CompilerError, not
//! silently vanish from the chunk ("Reject, don't fall back"): the lenient
//! `skipped` channel exists for known stdlib parity gaps only. Before this
//! gate, an unemittable user fn (e.g. a caller of a recursive bounded fn)
//! shipped as a missing global and died at runtime as a nil call.

mod common;
use common::{emit_mir, try_emit_module};

// `rec` is self-recursive AND bounded: never inlinable, no standalone emission,
// so `main`'s residual call can never resolve. This must be a compile error.
const RECURSIVE_BOUNDED_CALLER: &str = r#"
fn rec<I: Iterator<int>>(it: I) -> int {
    return rec(it)
}

fn main() {
    print(rec([1].iter()))
}
"#;

#[test]
fn user_fn_emit_failure_is_a_hard_error() {
    let err = try_emit_module(RECURSIVE_BOUNDED_CALLER)
        .expect_err("an unemittable user fn must reject, not silently skip");
    assert!(
        err.message.contains("rec"),
        "error should name the offending call chain: {}",
        err.message
    );
}

// The stdlib's known parity gaps (`flat_vec_new`/`grow`, generic bodies that
// cannot name a concrete ffi ctype) must stay lenient skips -- a plain program
// still emits.
#[test]
fn stdlib_parity_gaps_stay_lenient() {
    let emit = emit_mir("fn main() { print(1) }");
    assert!(
        emit.skipped.iter().all(|(name, _)| name.starts_with("flat_vec_new") || name.starts_with("grow")),
        "only the known stdlib gaps may skip: {:?}",
        emit.skipped
    );
}
