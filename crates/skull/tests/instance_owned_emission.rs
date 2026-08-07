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

/// SENTINEL -- flip when instance-owned definition emission lands (mono Stage 3
/// cut B-F, `docs/monomorphization-plan.md` S 11).
///
/// The same generic at two concrete types with the SAME ABI width. CORRECT
/// behavior is two definitions, one per instance, each call naming its own:
/// the two instances have different concrete bodies (`T` is `int` in one and
/// `string` in the other). TODAY nothing splats, so the conditional
/// specialization declines and ONE shared body mangled on the DECLARED
/// signature (`identity_T_ret_T`) serves both -- which is exactly the
/// declaration ownership Stage 3 cuts.
///
/// This is inverted to pin that. When the cut lands this goes red: replace the
/// `1`s with `2`s and delete this paragraph.
#[test]
fn same_abi_width_instances_share_one_declaration_body_sentinel() {
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
    assert_eq!(
        defs,
        vec!["identity_T_ret_T".to_string()],
        "PINNED BEHAVIOR (declaration ownership): both instances share one body \
         mangled on the DECLARED signature. If this is now two definitions, the \
         instance-owned cut landed -- flip to `defs.len() == 2`."
    );
    let called = callees_named_in(&lua, "main", "identity");
    assert_eq!(called.len(), 1, "PINNED: both calls name the one shared body, got {called:?}");
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

/// SENTINEL -- flip when instance-owned definition emission lands (same cut as
/// the sibling above).
///
/// A generic caller invoking a generic callee. CORRECT behavior is that the
/// callee instance is selected by the CALLER's instance: two `outer` instances
/// each reach their own `inner`, so there are two of each and each `outer` body
/// names its own. TODAY there is one `outer` and one `inner`, both mangled on
/// their declared signatures, and the single `outer` body names the single
/// `inner` -- the nested case of the same declaration ownership.
///
/// When the cut lands this goes red: assert two of each, and that the two
/// `outer` bodies name DIFFERENT `inner` symbols.
#[test]
fn a_nested_generic_caller_shares_one_callee_body_sentinel() {
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
    assert_eq!(outers, vec!["outer_T_ret_T".to_string()], "PINNED: one shared `outer`");
    assert_eq!(inners, vec!["inner_T_ret_T".to_string()], "PINNED: one shared `inner`");
    // The one `outer` body names the one `inner`. Read from the body, so this
    // still proves the call EDGE exists rather than that the name occurs.
    assert_eq!(
        callees_named_in(&lua, &outers[0], "inner"),
        vec!["inner_T_ret_T".to_string()],
        "PINNED: the shared `outer` names the shared `inner`. Two distinct names \
         here means the instance-owned cut landed -- flip this test."
    );
}
