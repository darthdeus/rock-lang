use crate::common::{bone_rejections, emit_module, function_body, run_main};

const CANONICAL_VALUES: &str = r#"
value ProbeUniform {
    radius: float,
    direction: Vec2,
    enabled: bool,
    count: int,
    packed: uint,
}

fn main() {
    let b = uniform_values(ProbeUniform {
        radius: 1.0,
        direction: v(2.0, 3.0),
        enabled: true,
        count: 7,
        packed: 0xffffffff,
    })
    print(b.fingerprint != 0)
    print(b.leaves.len())
}
"#;

#[test]
fn uniform_values_expands_to_canonical_leaves() {
    assert!(bone_rejections(CANONICAL_VALUES).is_empty());
    let lua = emit_module(CANONICAL_VALUES);
    let body = function_body(&lua, "main");
    assert!(!body.contains("uniform_values("));
    for forbidden in ["std140", "offset", "padding", "binding"] {
        assert!(!body.contains(forbidden));
    }
    for variant in ["Float", "Bool", "Int", "Uint"] {
        assert!(body.contains(&format!("__variant = (\"{variant}\")")));
    }
    assert_eq!(run_main(&lua), "true\n6");
}

const GLOBAL_SOURCE: &str = r#"
value Wide { a: Vec4, b: Vec4, c: Vec4, d: Vec4, m: Mat4 }

global BLOCK: Wide = Wide {
    a: v(1.0, 2.0, 3.0, 4.0), b: v(5.0, 6.0, 7.0, 8.0),
    c: v(9.0, 10.0, 11.0, 12.0), d: v(13.0, 14.0, 15.0, 16.0),
    m: Mat4 {
        c0: v(17.0, 0.0, 0.0, 0.0), c1: v(0.0, 18.0, 0.0, 0.0),
        c2: v(0.0, 0.0, 19.0, 0.0), c3: v(0.0, 0.0, 0.0, 20.0),
    },
}

fn pack_global(f: Ptr<float>, i: Ptr<int>) {
    pack_uniform_block(f, i, BLOCK)
}

fn main() {
    let f = native_buffer_float(32)
    let i = native_buffer_int(32)
    pack_global(f.ptr, i.ptr)
    print(f.get(0).unwrap())
    print(f.get(5).unwrap())
    print(f.get(15).unwrap())
    print(f.get(16).unwrap())
    print(f.get(31).unwrap())
}
"#;

#[test]
fn a_global_block_packs_one_leaf_at_a_time() {
    let lua = emit_module(GLOBAL_SOURCE);
    let body = function_body(&lua, "pack_global");
    let decl = body
        .lines()
        .find(|l| l.trim_start().starts_with("local "))
        .unwrap_or_else(|| panic!("no local declaration in:\n{body}"));
    let registers = decl
        .split(',')
        .filter(|n| n.trim().starts_with('r') || n.trim().starts_with("local r"))
        .count();
    assert!(registers < 32);
    assert_eq!(run_main(&lua), "1\n6\n16\n17\n20");
}
