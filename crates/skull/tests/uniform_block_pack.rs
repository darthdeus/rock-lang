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

const BLOOM: &str = r#"
value BloomBlurU { radius: float, direction: Vec2, resolution: float }

fn main() {
    let f = native_buffer_float(8)
    let i = native_buffer_int(8)
    let u = BloomBlurU { radius: 1.0, direction: v(2.0, 3.0), resolution: 4.0 }
    pack_uniform_block(f.ptr, i.ptr, u)
}
"#;

#[test]
fn bloom_blur_packs_as_unboxed_stores_at_its_std140_element_indices() {
    let lua = emit_module(BLOOM);
    let body = function_body(&lua, "main");
    assert!(!body.contains("pack_uniform_block"));
    assert!(!body.contains("BloomBlurU_lct") && !body.contains("BloomBlurU_ct"));
    for idx in ["[(0)] = ", "[(2)] = ", "[(3)] = ", "[(4)] = "] {
        assert!(body.contains(idx));
    }
    assert!(!body.contains("[(1)] = "));
}

#[test]
fn int_and_bool_leaves_store_through_the_other_view() {
    let lua = emit_module(
        r#"
value Flags { scale: float, on: bool, count: int }

fn main() {
    let f = native_buffer_float(8)
    let i = native_buffer_int(8)
    let u = Flags { scale: 1.5, on: true, count: 7 }
    pack_uniform_block(f.ptr, i.ptr, u)
}

"#,
    );
    let body = function_body(&lua, "main");
    let fbuf = body
        .lines()
        .find(|l| l.contains(r#"__rock_nb_alloc("float"#))
        .and_then(|l| l.split_whitespace().next())
        .expect("the float view's local");
    let ibuf = body
        .lines()
        .find(|l| l.contains(r#"__rock_nb_alloc("int"#))
        .and_then(|l| l.split_whitespace().next())
        .expect("the int view's local");
    assert_ne!(fbuf, ibuf);
    assert!(body.contains(&format!("{fbuf}[(0)] = (1.5)")));
    for store in [format!("{ibuf}[(1)] = "), format!("{ibuf}[(2)] = ")] {
        assert!(body.contains(&store));
    }
}

#[test]
fn uint_uniform_leaves_preserve_the_low_bits_in_the_int_view() {
    let src = r#"
value Packed { value: uint }

fn main() {
    let f = native_buffer_float(1)
    let i = native_buffer_int(1)
    pack_uniform_block(f.ptr, i.ptr, Packed { value: 0xffffffff })
    print(i[0])
}
"#;
    assert_eq!(run_main(&emit_module(src)), "-1");
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
