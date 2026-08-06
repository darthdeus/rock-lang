//! Control 2 of the 2.4 trigger (`docs/boundary-contracts.md`): the rule reads
//! the DECLARED SEMANTIC parameter or receiver type under the concrete
//! substitution, and a `Map<K, V>` is a container on the same terms `[T]` is.
//!
//! A `Map<K, V>` at a column-eligible V is not a bare table: it is
//! `{spine, data, free_indices, occupied_count}` over column storage
//! (`docs/nobox-map-value-design.md`). An opaque body handed one sees the
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
//! so they never reach the check at `:645`. `a_column_map_still_works_through_
//! the_stdlib_surface` is the control that keeps that true.

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

/// The same implicit receiver at a BOXED value: the identical method must stay
/// correct, or the rejection above is just "methods reject".
const IMPLICIT_RECEIVER_BOXED: &str = r#"
struct Boxy { s: string }

@nomangle inline(lua) fn Map<K, V>.probe(map) -> int = """
    local n = 0
    for k, v in pairs(map) do n = n + 1 end
    return n
"""

fn main() {
    let m: Map<string, Boxy> = map_new()
    m["a"] = Boxy { s: "z" }
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

/// Bounds the rule on the VALUE type. A boxed value keeps the bare table, so the
/// identical body must stay correct -- without this, a change that rejected
/// every `inline(lua)` Map param would leave the rest green and read as progress.
const BOXED_VALUE_ELEM: &str = r#"
struct Boxy { s: string }

@nomangle inline(lua) fn map_probe<V>(m: Map<string, V>) -> int = """
    local n = 0
    for k, v in pairs(m) do n = n + 1 end
    return n
"""

fn main() {
    let m: Map<string, Boxy> = map_new()
    m["a"] = Boxy { s: "z" }
    print("{map_probe(m)}")
}
"#;

/// Control 3's Map twin, and the one that fails OPEN. An `any` param claims no
/// representation, so it must still be admitted -- the rule may not read the
/// ARGUMENT's type. This goes red only if the rule over-fires.
const ANY_PARAM_PROBE: &str = r#"
value Q { x: float, y: float }

@nomangle inline(lua) fn map_shape(m: any) -> string = """
    if m.spine ~= nil then return "spine" end
    return "bare"
"""

fn main() {
    let m: Map<string, Q> = map_new()
    m["a"] = Q { x: 1.0, y: 2.0 }
    print("{map_shape(m)}")
}
"#;

/// The stdlib surface over the very same representation. Exempt by construction
/// (the intercept returns before the check), and this is what proves widening
/// the rule to `Map` did not take the stdlib down with it.
const STDLIB_SURFACE: &str = r#"
value Q { x: float, y: float }

fn main() {
    let m: Map<string, Q> = map_new()
    m["a"] = Q { x: 1.0, y: 2.0 }
    m["b"] = Q { x: 3.0, y: 4.0 }
    m.remove("a")
    print("{m.len()}{m.contains(\"b\")}{m.get(\"a\") == null}{m[\"b\"].x}")
}
"#;

fn run(src: &str, tag: &str) -> String {
    let lua = crate::common::try_emit_module(src).expect("must emit");
    crate::common::run_luajit(&format!("{lua}\nmain()\n"), tag)
}

fn rejection(src: &str) -> String {
    crate::common::try_emit_module(src)
        .expect_err("a column map at an opaque `Map<K, V>` param must reject")
        .message
}

/// CONTROL 2. Distinct from the explicitly-typed cases below: the receiver has
/// no written type at all, so this is the only case that proves the rule reads
/// the semantic declared type rather than the syntax.
#[test]
fn an_implicit_method_receiver_is_classified() {
    let msg = rejection(IMPLICIT_RECEIVER);
    assert!(
        msg.contains("column-stored `Map<_, Q>`") && msg.contains("probe"),
        "an implicit `Map<K, V>` receiver must be classified, got: {msg}"
    );
}

#[test]
fn an_implicit_method_receiver_at_a_boxed_value_is_correct() {
    assert_eq!(
        run(IMPLICIT_RECEIVER_BOXED, "inline_map_recv_boxed"),
        "1",
        "the rejection is about the value's storage, not about method syntax"
    );
}

#[test]
fn a_generic_inline_lua_body_over_a_column_map_rejects() {
    let msg = rejection(USER_BODY_GENERIC);
    assert!(
        msg.contains("column-stored `Map<_, Q>`") && msg.contains("map_probe"),
        "the rejection must name the value type and the callee, got: {msg}"
    );
}

#[test]
fn a_non_generic_inline_lua_body_over_a_column_map_rejects_identically() {
    assert!(rejection(USER_BODY_CONCRETE).contains("column-stored `Map<_, Q>`"));
}

#[test]
fn a_boxed_value_through_the_same_body_is_correct() {
    assert_eq!(
        run(BOXED_VALUE_ELEM, "inline_map_boxed"),
        "1",
        "a boxed value keeps the bare table, so the same body must stay correct"
    );
}

/// Fails OPEN: red only if the rule over-fires by reading the argument type.
#[test]
fn an_any_param_still_admits_a_column_map() {
    assert_eq!(
        run(ANY_PARAM_PROBE, "inline_map_any"),
        "spine",
        "an `any` param claims no representation, so it must still be admitted"
    );
}

#[test]
fn a_column_map_still_works_through_the_stdlib_surface() {
    assert_eq!(
        run(STDLIB_SURFACE, "inline_map_stdlib"),
        "1truetrue3",
        "the intercepted stdlib names are exempt by construction"
    );
}
