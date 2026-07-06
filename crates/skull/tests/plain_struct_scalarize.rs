//! Codegen-shape contract: a plain `struct` whose only uses are field reads (and
//! field writes) must NOT survive as a `{__ty=..}` Lua table after inlining.
//!
//! skull unboxes `value` structs (opt #1, `scalarize_value_cells`) but used to
//! emit plain structs as a Lua table and lean on LuaJIT's allocation sinking
//! (SINK) to elide them. SINK only fires on a compiled trace -- in the
//! interpreter (mag's aborted HUD/text loops) the table allocates every
//! iteration. rock scalarizes plain structs into per-field locals at codegen, so
//! the table never exists and rock pays 0 even interpreted. This pins skull to
//! that parity: a non-escaping plain-struct cell is split into per-field cells.
//!
//! Mirrors `value_scalarize.rs`; the difference is plain structs are MUTABLE
//! (field WRITES are legal and become field-cell stores), and a whole-struct use
//! (pass-by-value, `==`, return, store as another struct's field) must keep the
//! table.

use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_host::stdlib_loader::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(format!("<stdlib:{}>", f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect()
});

fn emit_module(src: &str) -> String {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    skull::mir_emit::emit_workspace_mir(&ws, mid).expect("emit").lua
}

fn function_body<'a>(lua: &'a str, name: &str) -> &'a str {
    let head = format!("function {name}");
    let start = lua.find(&head).unwrap_or_else(|| panic!("no `{head}` in:\n{lua}"));
    let rest = &lua[start..];
    let end = rest.find("\nend\n").map(|e| e + 5).unwrap_or(rest.len());
    &rest[..end]
}

// A plain struct returned from an `@inline` helper and field-read across branch
// arms (other blocks) -- the same single-block-value limitation that forces
// `value` structs to allocate. The table must scalarize into per-field cells.
const PLAIN_FIELD_LOOP: &str = r#"
struct Props { a: int, b: int, c: int }

@inline fn mk(x: int) -> Props { return Props { a: x, b: x * 2, c: x * 3 } }

fn hot(w: int, h: int) -> int {
    let acc = 0
    for gy in 0..h {
        for gx in 0..w {
            let p = mk(gx + gy)
            let kind = ((gx * 7) + (gy * 13)) % 3
            if kind == 0 {
                acc = (acc + (p.a * 2)) - p.b
            } else if kind == 1 {
                acc = (acc - p.b) + p.c
            } else {
                acc = (acc + p.a) + p.c
            }
        }
    }
    return acc
}

fn main() {
    let _ = hot(2, 2)
}
"#;

#[test]
fn plain_struct_field_reads_dont_materialize_table() {
    let lua = emit_module(PLAIN_FIELD_LOOP);
    let hot = function_body(&lua, "hot");
    assert!(
        !hot.contains("__ty = \"Props\""),
        "Props table survived inlining in hot() (should scalarize away):\n{hot}"
    );
}

// Plain structs are mutable: a local struct whose fields are WRITTEN and then
// read must still scalarize -- the field write becomes a per-field-cell store.
const PLAIN_MUTATE_LOOP: &str = r#"
struct Acc { sum: int, n: int }

fn hot(w: int) -> int {
    let total = 0
    for i in 0..w {
        let a = Acc { sum: 0, n: 0 }
        a.sum = i * 2
        a.n = i
        total = total + (a.sum + a.n)
    }
    return total
}

fn main() {
    let _ = hot(4)
}
"#;

#[test]
fn plain_struct_field_writes_scalarize() {
    let lua = emit_module(PLAIN_MUTATE_LOOP);
    let hot = function_body(&lua, "hot");
    assert!(
        !hot.contains("__ty = \"Acc\""),
        "Acc table survived (mutated plain struct should scalarize):\n{hot}"
    );
}

// A plain struct used as a WHOLE (passed by value to a function) must keep its
// table -- the call escapes the aggregate, so SROA must not split the cell (a
// half-rewrite would read never-stored per-field cells). Runs the field read
// alongside to force the candidate shape.
const PLAIN_WHOLE_USE: &str = r#"
struct Box { w: int, h: int }

fn area(b: Box) -> int { return b.w * b.h }

fn use_it() -> int {
    let b = Box { w: 3, h: 4 }
    let direct = b.w
    return area(b) + direct
}

fn main() {
    let _ = use_it()
}
"#;

#[test]
fn plain_struct_whole_use_keeps_table() {
    let lua = emit_module(PLAIN_WHOLE_USE);
    let body = function_body(&lua, "use_it");
    assert!(
        body.contains("__ty = \"Box\""),
        "a plain struct passed by value must keep its table:\n{body}"
    );
    assert!(
        body.contains(".w"),
        "the field read of a non-splittable plain struct must project off it:\n{body}"
    );
}
