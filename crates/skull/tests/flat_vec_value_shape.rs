use crate::common::{emit_module, function_body};

const FLAT_VEC_LOOP: &str = r#"
value V2 { x: float, y: float }

fn fill(n: int) -> float {
    let fv: FlatVec<V2> = flat_vec_new(4)
    for i in 0..n {
        fv.push(V2 { x: 1.0, y: 2.0 })
    }
    let acc = 0.0
    for i in 0..fv.len {
        acc = acc + fv.get(i).x
    }
    return acc
}

fn main() { let _ = fill(8) }
"#;

#[test]
fn flat_vec_push_read_loop_is_zero_box() {
    let lua = emit_module(FLAT_VEC_LOOP);
    let body = function_body(&lua, "fill");

    assert!(!body.contains("V2_ct("));
    assert!(!body.contains("__nb_checked_get("));
    assert!(body.contains("__nb_checked_index("));
    assert!(body.contains(r#"__rock_nb_alloc("V2","#));
    assert!(!body.contains("flat_vec_new") && !body.contains(" push("));
}
