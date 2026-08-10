//! Emission is owned by the INSTANCE, not by the declaration (plan S 9,
//! Stage 3).
//!
//! Every assertion here reads emitted DEFINITION HEADERS or a CALL BODY, never
//! a bare name match: the callee's own definition line contains its symbol, so
//! `lua.contains("identity_int")` is green whether or not any call site reaches
//! it. Definitions are counted from `\nfunction <sym>(` headers; call spellings
//! are read out of the caller's body via `function_body`.

mod common;
use common::{emit_module, function_body, ws_with_stdlib};

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

/// The `iter` symbol the body of `caller` calls, whatever it is spelled.
///
/// Deliberately NOT filtered to known definitions the way `callees_named_in`
/// is: the property here is that the spelling names something that EXISTS, so
/// filtering by the definition set first would answer the question by
/// construction and pass on a call to a nil global.
fn iter_called_in(lua: &str, caller: &str) -> String {
    let body = function_body(lua, caller);
    let at = body.find("iter_").unwrap_or_else(|| panic!("no `iter` call in `{caller}`:\n{body}"));
    let rest = &body[at..];
    rest[..rest.find('(').expect("a call has `(`")].to_string()
}

/// An `iter` witness names an INSTANCE in both call forms it has.
///
/// Two forms reach the generic stdlib `iter`: a `for` head's witness, and the
/// implicit iterable->iterator coercion at a bounded call argument
/// (`a.map(..)`, where `a` is `[T]` and `map` takes an `Iterator`). Under
/// instance-owned emission the generic declaration has no body, so a witness
/// that records identity without DELIVERING its instantiation spells a name
/// nothing defines -- a nil global at runtime, with no compiler diagnostic,
/// because the Lua emitter does not check that a call resolves.
///
/// The `for` arm is the control: it delivers, so a failure in the coercion arm
/// is about the call FORM and not about `iter` witnesses in general.
#[test]
fn both_iter_witness_forms_name_a_definition_that_exists() {
    let _inline = common::InlineOffGuard::on();
    let coerced = emit_module(
        r#"
fn main() {
    let a: [int] = [1, 2]
    print("{a.map(_ * 2).sum()}")
}
"#,
    );
    let looped = emit_module(
        r#"
fn main() {
    let a: [int] = [1, 2]
    let s = 0
    for v in a.iter().map(_ * 2) { s = s + v }
    print("{s}")
}
"#,
    );
    for (form, lua) in [("implicit coercion", &coerced), ("`for` head", &looped)] {
        let called = iter_called_in(lua, "main");
        assert!(
            definition_symbols(lua).iter().any(|d| *d == called),
            "the {form} witness calls `{called}`, which no definition defines"
        );
    }
}

/// **The first coherent E1 landing: production emission is DRIVEN by its own
/// authoritative graph, not merely capable of building one beside itself.**
///
/// This is the RED that a lazy `Option<InstanceGraph>` plus a private
/// discovery walk (`LuaReach`, deleted -- population is now driven by
/// `cache.lua_view` end to end) fails and a production-owned graph/view
/// passes. It proves, in one fixture:
///
/// - `emit_full`'s cache always carries a graph (never absent) whose instance
///   population is non-empty.
/// - the Lua artifact view realized from that SAME graph has a non-zero
///   retained Rock-instance population.
/// - every emitted Rock DEFINITION symbol corresponds to one of the view's
///   retained instances (root-only would fail here: `leaf_only_reached_via_helper`
///   is reached transitively through `helper`, never rooted directly, and a
///   root-only population would omit it).
/// - a target BOUNDARY (`print`, `extern(lua)`) is never emitted as a Rock
///   definition, even though it is retained (as a boundary, not an instance).
/// - both a generic instance (`identity<int>`, `identity<string>`) and a
///   non-generic one (`helper`) participate.
#[test]
fn full_emission_is_driven_by_its_own_production_graph() {
    let ws = ws_with_stdlib(
        r#"
fn identity<T>(x: T) -> T { return x }

fn leaf_only_reached_via_helper() -> int { return 99 }

fn helper() -> int {
    return leaf_only_reached_via_helper()
}

fn main() {
    let a = identity(7)
    let b = identity("s")
    let h = helper()
    print("{a} {b} {h}")
}
"#,
    );
    let mid = skull::user_module_id(&ws).expect("user module");
    let (emit, cache) = skull::mir_emit::emit_full(&ws, mid).expect("emit_full");

    // The graph is authoritative and non-empty -- not `None`, not deferred.
    let graph = cache.graph();
    assert!(!graph.instances.is_empty(), "the eager graph must be built by emit_full, not deferred");

    // The Lua view realized from that same graph retains a non-zero Rock
    // population.
    let view = cache.lua_view();
    assert!(!view.instances.is_empty(), "the Lua artifact view must retain Rock instances");

    // Both a generic and a non-generic instance participate in the retained
    // population.
    let generic_count = view.instances.iter().filter(|i| !i.args.is_empty()).count();
    let nongeneric_count = view.instances.iter().filter(|i| i.args.is_empty()).count();
    assert!(generic_count >= 2, "expected >=2 generic instances (identity<int>, identity<string>), got {generic_count}");
    assert!(nongeneric_count >= 1, "expected >=1 non-generic instance, got {nongeneric_count}");

    // Retention and standalone delivery are separate: an instance whose call
    // was spliced can remain in the view without a definition of its own.
    // Every retained instance that is delivered must still name an emitted
    // definition, and the transitively reached leaf is one such body.
    let defs = definition_symbols(&emit.lua);
    let mut checked_transitive_leaf = false;
    let mut retained_without_definition = 0;
    let mut checked_residual_calls = 0;
    for inst in &view.instances {
        let sym = view.symbols.get(inst).expect("every retained instance has a symbol");
        if !defs.contains(sym) {
            retained_without_definition += 1;
        }
        if graph.logical_symbol(&ws, inst).contains("leaf_only_reached_via_helper") {
            checked_transitive_leaf = true;
            assert!(defs.contains(sym), "the transitively reached leaf was not delivered: `{sym}`");
        }
    }
    for caller in &defs {
        let body = function_body(&emit.lua, caller);
        for (inst, sym) in &view.symbols {
            if !view.instances.contains(inst) {
                continue;
            }
            if body.contains(&format!("{sym}(")) {
                checked_residual_calls += 1;
                assert!(
                    defs.contains(sym),
                    "a residual call names Rock instance `{sym}` without an emitted definition"
                );
            }
        }
    }
    assert!(
        checked_transitive_leaf,
        "fixture defect: the transitively-reached leaf was not retained at all"
    );
    assert!(
        retained_without_definition > 0,
        "the fixture did not expose the retention/delivery distinction"
    );
    assert!(
        checked_residual_calls > 0,
        "fixture defect: no retained Rock call crossed a residual emission boundary"
    );

    // A boundary (`print`) is retained as a boundary, never as a Rock
    // definition: it must not appear in `view.instances`, and its logical
    // symbol must not be an emitted definition header.
    let boundary = view
        .boundaries
        .keys()
        .find(|i| graph.logical_symbol(&ws, i).contains("print"))
        .expect("print must be retained as a boundary");
    assert!(
        !view.instances.contains(boundary),
        "a boundary must never be a retained Rock instance"
    );
    let boundary_sym = graph.logical_symbol(&ws, boundary);
    assert!(
        !defs.iter().any(|d| *d == boundary_sym),
        "a boundary (`{boundary_sym}`) must not be emitted as a Rock definition"
    );
}
