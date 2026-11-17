use crate::common::{callees_named_in, definitions_of, emit_module};

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
