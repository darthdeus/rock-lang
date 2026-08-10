//! Total valid-type identity (`docs/monomorphization-plan.md` S 2a/S 2b).
//!
//! `unit`, `any`, `never` and a function type are VALID logical types. `MonoTy`
//! has a constructor for each, so a generic bound to one has a name -- the
//! question is only whether the specialization key admits it. While it does not,
//! that slot is `None`, and `None` is not "some type": it is the ABSENCE of one,
//! shared by every instantiation that lands in it.
//!
//! So the defect is an identity COLLISION, not a missing optimization. The
//! assertions here read emitted definition headers and caller bodies (the
//! `instance_owned_emission` discipline) rather than a bare name, because a
//! callee's own definition line contains its symbol.

mod common;
use common::{emit_module, function_body};

/// Every emitted top-level definition symbol, from its `function <sym>(` header.
fn definition_symbols(lua: &str) -> Vec<String> {
    lua.match_indices("\nfunction ")
        .map(|(i, m)| {
            let rest = &lua[i + m.len()..];
            rest[..rest.find('(').expect("fn header has `(`")].to_string()
        })
        .collect()
}

/// The definition symbols derived from source fn `name`, excluding synthetic
/// children (`__lambda0`, `__step`), which are a different obligation.
fn definitions_of(lua: &str, name: &str) -> Vec<String> {
    definition_symbols(lua)
        .into_iter()
        .filter(|s| s.starts_with(name) && !s.contains("__"))
        .collect()
}

/// The definition symbols `caller`'s body actually CALLS, restricted to
/// derivations of `callee`.
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

/// Plan S 2a's collision, realized.
///
/// `pick<A, B>` is specialized because `A = Vec2` is a value struct (the
/// `any_value` trigger), so all three calls DO get a specialized body. `B` is
/// bound to `unit`, to `any` and to `fn(int) -> int` -- three different types
/// whose key slot is the same `None`, so all three instantiations mangle to one
/// symbol and one emitted body serves types with three different
/// representations.
///
/// The control that makes this about `B` and not about specialization in
/// general is `varying_the_admitted_slot_already_emits_three_definitions`
/// below: the identical shape varying `A` over three ADMITTED types emits
/// three. If both were one, the fixture would be failing to specialize at all.
#[test]
fn three_unadmitted_bindings_are_three_instances() {
    let lua = emit_module(
        r#"
fn pick<A, B>(a: A, b: B) -> A { return a }
fn nothing() {}
fn twice(n: int) -> int { return n * 2 }

fn main() {
    let v = Vec2 { x: 3, y: 5 }
    let anyv: any = 1
    let f: fn(int) -> int = twice
    let p = pick(v, nothing())
    let q = pick(v, anyv)
    let r = pick(v, f)
    print("{p.x} {q.x} {r.x}")
}
"#,
    );
    let defs = definitions_of(&lua, "pick");
    let called = callees_named_in(&lua, "main", "pick");
    assert_eq!(
        called.len(),
        3,
        "the three calls must name three DIFFERENT symbols -- one symbol for all \
         three is the S 2a collision, because a `None` key slot is the absence of \
         a type and every unadmitted binding shares it; got {called:?} of {defs:?}"
    );
    // Each spelling must be a definition this module actually emits. Three
    // distinct spellings over two definitions would be the same collision one
    // level down -- a call naming a body that is not there.
    for sym in &called {
        assert!(
            defs.contains(sym),
            "`main` calls `{sym}`, which no emitted definition header declares; \
             got definitions {defs:?}"
        );
    }
}

/// The admission CONTROL for the test above: the same shape, varying the slot
/// over three types the key already admits. This must emit three definitions
/// both before and after S 2a, so it says the fixture specializes at all and
/// that a `3` above is about the unadmitted constructors specifically.
#[test]
fn varying_the_admitted_slot_already_emits_three_definitions() {
    let lua = emit_module(
        r#"
fn pick<A, B>(a: A, b: B) -> A { return a }

fn main() {
    let v = Vec2 { x: 3, y: 5 }
    let p = pick(v, 1)
    let q = pick(v, "s")
    let r = pick(v, true)
    print("{p.x} {q.x} {r.x}")
}
"#,
    );
    let defs = definitions_of(&lua, "pick");
    let called = callees_named_in(&lua, "main", "pick");
    assert_eq!(
        called.len(),
        3,
        "int / string / bool are admitted today, so the three calls already name \
         three symbols; got {called:?} of {defs:?}"
    );
}
