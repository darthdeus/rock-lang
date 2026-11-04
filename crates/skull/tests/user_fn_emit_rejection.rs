//! A USER fn whose body fails to emit must reject with a CompilerError, not
//! silently vanish from the chunk ("Reject, don't fall back"): the lenient
//! `skipped` channel exists for known stdlib parity gaps only. Before this
//! gate, an unemittable user fn (e.g. a caller of a recursive bounded fn)
//! shipped as a missing global and died at runtime as a nil call.
use crate::common::{emit_mir, try_emit_module};

// CHANGING-INSTANCE RECURSION: `rec<T>` calls itself at `[T]`, so every call
// needs a body for a strictly deeper type and the chain never closes. Chosen
// because it is unemittable BY DECISION and permanently so -- plan S 0h decided
// changing-instance recursion is a REJECT -- rather than because some gap has
// not been closed yet.
//
// The previous fixture was a self-recursive BOUNDED fn, justified as having "no
// standalone emission". D-1 gave bounded bodies exactly that, so the fixture
// compiled fine and the test failed open. That is the trap `CLAUDE.md` names:
// a test whose fixture is an open gap goes green when the gap closes.
const POLYMORPHIC_RECURSION: &str = r#"
fn rec<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    return rec([x], n - 1)
}

fn main() {
    print("{rec(1, 3)}")
}
"#;

// The admission control for the rejection: recursion whose type arguments do
// NOT change is same-instance recursion, which is legal and must keep
// compiling. Without it the rejection above could be "generic recursion is
// refused", which would be a different and much worse rule.
const SAME_INSTANCE_RECURSION: &str = r#"
fn countdown<T>(x: T, n: int) -> int {
    if n <= 0 { return 0 }
    return 1 + countdown(x, n - 1)
}

fn main() {
    print("{countdown(1, 5)}")
}
"#;

/// An unemittable user fn REJECTS, and under E1 the rejection comes from the
/// INSTANCE -- there is no declaration body left to reject from.
///
/// Before this, emission did not reject: it HUNG. Each call demanded a strictly
/// deeper instantiation, every node was distinct, and the reachability walk's
/// own dedup could never close the loop. Pre-E1 the shared generic body absorbed
/// the recursion, so removing that body is what turned a compiling program into
/// a non-terminating compiler -- measured at the CLI, `rock run` timed out at 60s
/// with no output while `--backend bone` type-checked it in 14ms.
#[test]
fn user_fn_emit_failure_is_a_hard_error() {
    let err = try_emit_module(POLYMORPHIC_RECURSION)
        .expect_err("an unemittable user fn must reject, not silently skip");
    assert!(
        err.message.contains("rec"),
        "error should name the offending call chain: {}",
        err.message
    );
    assert!(
        err.message.contains("polymorphic recursion"),
        "the rejection must name the CAUSE, so it is distinguishable from any \
         other emit failure that happens to mention `rec`: {}",
        err.message
    );
}

/// Same-instance recursion still emits. This is the half that makes the
/// rejection a statement about GROWTH rather than about recursion.
#[test]
fn same_instance_recursion_still_emits() {
    if let Err(e) = try_emit_module(SAME_INSTANCE_RECURSION) {
        panic!(
            "recursion whose type arguments do not change must still emit: {}",
            e.message
        );
    }
}

// The stdlib's known parity gaps (`flat_vec_new`/`grow`, generic bodies that
// cannot name a concrete ffi ctype) must stay lenient skips -- a plain program
// still emits.
#[test]
fn stdlib_parity_gaps_stay_lenient() {
    let emit = emit_mir("fn main() { print(1) }");
    assert!(
        emit.skipped
            .iter()
            .all(|(name, _)| name.starts_with("flat_vec_new") || name.starts_with("grow")),
        "only the known stdlib gaps may skip: {:?}",
        emit.skipped
    );
}

/// A call that reaches a GENERIC DECLARATION's body rejects, rather than
/// emitting a call to a symbol nothing defines.
///
/// E1 left a generic declaration with no body at all, so its mangled name is
/// never written. The Lua emitter had no check for this: it wrote the call and
/// the program died at runtime on `attempt to call global '..' (a nil value)`,
/// with no compiler diagnostic -- while the JS emitter rejected the same
/// condition with a span. That asymmetry is what hid the class on the backend
/// the games ship on, and it cost 59 Rock `@test`s before the two witness call
/// forms were fixed.
///
/// There is no SOURCE that produces this today, which is the point: every known
/// producer is fixed. So the gate is verified by MUTATION instead -- deleting
/// the implicit-iter coercion's `spec_bindings` delivery in
/// `skull_mir::lower::expr` reintroduces it, and emission then reports
/// ``iter_arr_T` is reached at its DECLARATION` where it
/// previously produced a running program that crashed. The message deliberately
/// carries no `internal:` label: an unconstrained type argument lands on the
/// same arm and is a PROGRAM error, so naming it a compiler invariant failure
/// would send the reader to the wrong place. This test pins the arm
/// that must keep the rejection reachable: an ordinary generic call still emits,
/// so the gate cannot be satisfied by refusing generics outright.
#[test]
fn an_ordinary_generic_call_still_emits() {
    let src = r#"
fn ident<T>(x: T) -> T { return x }

fn main() {
    print("{ident(1)} {ident(\"s\")}")
}
"#;
    if let Err(e) = try_emit_module(src) {
        panic!(
            "a generic call that DELIVERS its instantiation must emit -- the \
             declaration-body gate must not refuse generics outright: {}",
            e.message
        );
    }
}
