//! Control 2 of the 2.4 trigger: the rule reads
//! the DECLARED SEMANTIC parameter or receiver type under the concrete
//! substitution, and a `Map<K, V>` is a container on the same terms `[T]` is.
//!
//! A `Map<K, V>` at a column-eligible V is not a bare table: it is
//! `{spine, data, free_indices, occupied_count}` over column storage
//!. An opaque body handed one sees the
//! WRAPPER -- `pairs(m)` walks four bookkeeping fields instead of the entries,
//! which is a silent wrong answer, not a crash.
//!
//! The Vec axis of this rule is `inline_lua_column_container_param_sentinel.rs`;
//! this is its Map twin. The two share `reject_column_container_opaque_args` and
//! must reject on the same terms, so a change that fixes one and not the other
//! is the finding.
//!
//! The intercepted stdlib names (`get`, `contains`, `len`, `clear`,
//! `table_unset`, `map_new`) are exempt BY CONSTRUCTION rather than by an
//! exception here: `lower_call` replaces them at `call.rs:138-210` and returns,
//! so they never reach the check at `:645`. The boxed-value, `any` and stdlib
//! surface controls are in `tests/maps/opaque_map_param_controls.rock`.
use crate::common::emit_rejection;

/// Control 2 proper: the receiver is IMPLICIT. `probe` writes no type for
/// `map`, so only the semantic `param_decls` slot 0 carries `Map<K, V>` -- a
/// rule reading written syntax sees nothing here. Called through method syntax.
/// The name is deliberately not one the intercept consumes (`get`, `contains`,
/// `len`, `clear`, `table_unset`, `map_new`), or it would never reach the check.
const IMPLICIT_RECEIVER: &str = r#"
value Q { x: float, y: float }

@nomangle inline(lua) fn Map<K, V>.probe(map) -> int = """
    local n = 0
    for k, v in pairs(map) do n = n + 1 end
    return n
"""

fn main() {
    let m: Map<string, Q> = map_new()
    m["a"] = Q { x: 1.0, y: 2.0 }
    print("{m.probe()}")
}
"#;

const USER_BODY_GENERIC: &str = r#"
value Q { x: float, y: float }

@nomangle inline(lua) fn map_probe<V>(m: Map<string, V>) -> int = """
    local n = 0
    for k, v in pairs(m) do n = n + 1 end
    return n
"""

fn main() {
    let m: Map<string, Q> = map_new()
    m["a"] = Q { x: 1.0, y: 2.0 }
    print("{map_probe(m)}")
}
"#;

/// The discriminant, mirroring the Vec sentinel's: NOT generic, fails the same
/// way, so the specialization bail-out is not the cause.
const USER_BODY_CONCRETE: &str = r#"
value Q { x: float, y: float }

@nomangle inline(lua) fn map_probe(m: Map<string, Q>) -> int = """
    local n = 0
    for k, v in pairs(m) do n = n + 1 end
    return n
"""

fn main() {
    let m: Map<string, Q> = map_new()
    m["a"] = Q { x: 1.0, y: 2.0 }
    print("{map_probe(m)}")
}
"#;

/// CONTROL 2. Distinct from the explicitly-typed cases below: the receiver has
/// no written type at all, so this is the only case that proves the rule reads
/// the semantic declared type rather than the syntax.
#[test]
fn an_implicit_method_receiver_is_classified() {
    let msg = emit_rejection(IMPLICIT_RECEIVER);
    assert!(
        msg.contains("column-stored `Map<_, Q>`") && msg.contains("probe"),
        "an implicit `Map<K, V>` receiver must be classified, got: {msg}"
    );
}

#[test]
fn a_generic_inline_lua_body_over_a_column_map_rejects() {
    let msg = emit_rejection(USER_BODY_GENERIC);
    assert!(
        msg.contains("column-stored `Map<_, Q>`") && msg.contains("map_probe"),
        "the rejection must name the value type and the callee, got: {msg}"
    );
}

#[test]
fn a_non_generic_inline_lua_body_over_a_column_map_rejects_identically() {
    assert!(emit_rejection(USER_BODY_CONCRETE).contains("column-stored `Map<_, Q>`"));
}
