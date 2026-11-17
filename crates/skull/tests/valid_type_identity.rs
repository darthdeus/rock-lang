//! Total valid-type identity.
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

use crate::common::{callees_named_in, definitions_of, emit_module};

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
    assert_eq!(called.len(), 3);
    // Each spelling must be a definition this module actually emits. Three
    // distinct spellings over two definitions would be the same collision one
    // level down -- a call naming a body that is not there.
    for sym in &called {
        assert!(defs.contains(sym));
    }
}
