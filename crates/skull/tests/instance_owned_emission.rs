//! Emission is owned by the INSTANCE, not by the declaration (plan S 9,
//! Stage 3).
//!
//! Every assertion here reads emitted DEFINITION HEADERS or a CALL BODY, never
//! a bare name match: the callee's own definition line contains its symbol, so
//! `lua.contains("identity_int")` is green whether or not any call site reaches
//! it. Definitions are counted from `\nfunction <sym>(` headers; call spellings
//! are read out of the caller's body via `function_body`.

mod common;
use common::{emit_module, function_body};

/// Every emitted top-level definition symbol, from its `function <sym>(`
/// header. The header IS the definition, so this cannot be satisfied by a
/// mention inside some other body.
fn definition_symbols(lua: &str) -> Vec<String> {
    lua.match_indices("\nfunction ")
        .map(|(i, m)| {
            let rest = &lua[i + m.len()..];
            rest[..rest.find('(').expect("fn header has `(`")].to_string()
        })
        .collect()
}

/// The definition symbols derived from source fn `name` -- its own mangling and
/// any per-instance derivation of it. Excludes synthetic children (`__lambda0`,
/// `__step`), which are a different obligation.
fn definitions_of(lua: &str, name: &str) -> Vec<String> {
    definition_symbols(lua)
        .into_iter()
        .filter(|s| s.starts_with(name) && !s.contains("__"))
        .collect()
}

/// The definition symbols the body of `caller` actually CALLS, restricted to
/// derivations of `callee`. Read from the caller's emitted body, so a
/// definition that exists but is never reached does not satisfy it.
fn callees_named_in(lua: &str, caller: &str, callee: &str) -> Vec<String> {
    let body = function_body(lua, caller);
    let mut out: Vec<String> = definitions_of(lua, callee)
        .into_iter()
        .filter(|s| body.contains(&format!("{s}(")))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Two instances of one generic whose concrete ABI widths DIFFER: `T = int` is
/// one boxed slot, `T = Vec2` is two scalar lanes. Each needs its own
/// definition, and each call site must name its own.
#[test]
fn two_abi_widths_emit_two_definitions_each_called_by_name() {
    let lua = emit_module(
        r#"
fn identity<T>(x: T) -> T { return x }

fn main() {
    let a = identity(7)
    let v = identity(Vec2 { x: 3, y: 5 })
    print("{a} {v.x}")
}
"#,
    );
    let defs = definitions_of(&lua, "identity");
    assert_eq!(defs.len(), 2, "one definition per instance, got {defs:?}");
    let called = callees_named_in(&lua, "main", "identity");
    assert_eq!(called.len(), 2, "each call names its own instance, got {called:?} of {defs:?}");
}

/// The same generic at two concrete types with the SAME ABI width. Identity is
/// the INSTANCE, not the wire: `T = int` and `T = string` are two concrete
/// bodies whatever they splat to, so nothing about equal widths lets one
/// definition serve both.
///
/// This is the case declaration ownership got wrong, and it is why width is not
/// the discriminant -- its sibling above already passed on differing widths.
#[test]
fn two_same_abi_width_instances_emit_two_definitions() {
    let lua = emit_module(
        r#"
fn identity<T>(x: T) -> T { return x }

fn main() {
    let a = identity(7)
    let b = identity("hi")
    print("{a} {b}")
}
"#,
    );
    let defs = definitions_of(&lua, "identity");
    assert_eq!(defs.len(), 2, "one definition per instance, got {defs:?}");
    assert!(
        !defs.contains(&"identity_T_ret_T".to_string()),
        "no body is mangled on the DECLARED signature -- there is no shared \
         generic snapshot to be one, got {defs:?}"
    );
    let called = callees_named_in(&lua, "main", "identity");
    assert_eq!(called.len(), 2, "each call names its own instance, got {called:?} of {defs:?}");
}

/// ADMISSION CONTROL for both cases above. A non-generic declaration has one
/// instance with an empty substitution and must still emit EXACTLY once, under
/// its ordinary declaration mangling, called by that name. Without this, "two
/// definitions per generic" is equally satisfied by an emitter that renamed or
/// duplicated everything.
#[test]
fn a_non_generic_declaration_emits_exactly_once() {
    let lua = emit_module(
        r#"
fn plain(x: int) -> int { return x + 1 }

fn main() {
    print("{plain(7)}")
}
"#,
    );
    let defs = definitions_of(&lua, "plain");
    assert_eq!(defs, vec!["plain_int_ret_int".to_string()], "non-generic emits once, unrenamed");
    assert_eq!(callees_named_in(&lua, "main", "plain"), vec!["plain_int_ret_int".to_string()]);
}

/// A generic caller invoking a generic callee: the callee instance is selected
/// by the CALLER's instance. Two `outer` instances each reach their own
/// `inner`, so there are two of each.
///
/// The load-bearing arm is the LAST one. Two of each is equally satisfied by an
/// emitter that duplicated bodies without threading the caller's substitution
/// into the callee it names -- both `outer`s naming one `inner` would still
/// count 2 and 2. Asserting the two spellings DIFFER is what says the selection
/// happened.
#[test]
fn a_nested_generic_caller_names_its_own_callee_instance() {
    let lua = emit_module(
        r#"
fn inner<T>(x: T) -> T { return x }
fn outer<T>(x: T) -> T { return inner(x) }

fn main() {
    let a = outer(7)
    let b = outer("hi")
    print("{a} {b}")
}
"#,
    );
    let outers = definitions_of(&lua, "outer");
    let inners = definitions_of(&lua, "inner");
    assert_eq!(outers.len(), 2, "one `outer` per instance, got {outers:?}");
    assert_eq!(inners.len(), 2, "one `inner` per instance, got {inners:?}");
    // Read from each caller's body, so this proves the call EDGE rather than
    // that the name occurs somewhere in the chunk.
    let first = callees_named_in(&lua, &outers[0], "inner");
    let second = callees_named_in(&lua, &outers[1], "inner");
    assert_eq!(first.len(), 1, "each `outer` names exactly one `inner`, got {first:?}");
    assert_eq!(second.len(), 1, "each `outer` names exactly one `inner`, got {second:?}");
    assert_ne!(
        first, second,
        "the two `outer` instances must name DIFFERENT `inner` instances -- \
         naming one shared `inner` is declaration ownership one level down"
    );
}
