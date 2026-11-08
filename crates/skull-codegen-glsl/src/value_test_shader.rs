use super::*;
use ustr::Ustr;

fn build_ws(src: &str) -> (Workspace, ModuleId) {
    let mut modules: Vec<skull_parser::Ast> = rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            let id = Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str());
            skull_parser::parse_module(f.contents, id).expect("stdlib parse")
        })
        .collect();
    modules.push(skull_parser::parse_module(src, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic_pass");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("mid");
    (ws, mid)
}

#[test]
fn int_return_wraps_root_and_writes_int_output() {
    let (ws, mid) = build_ws("fn t() -> int { return min(3, 5) }");
    let s = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect("emit");
    assert_eq!(s.ret, ValueTestRet::Int);
    assert!(
        s.fragment.starts_with("#version 330 core\n"),
        "{}",
        s.fragment
    );
    assert!(
        s.fragment
            .contains("layout(location = 0) out int _rock_value_out;"),
        "{}",
        s.fragment
    );
    assert!(
        s.fragment.contains("_rock_value_out = t_0();"),
        "{}",
        s.fragment
    );
    assert!(
        s.fragment.contains("int t_0()"),
        "callee def present: {}",
        s.fragment
    );
    assert!(s.vertex.contains("gl_VertexID"), "{}", s.vertex);
}

#[test]
fn float_return_uses_r32f_shape() {
    let (ws, mid) = build_ws("fn t() -> float { return clamp(9.0, 0.0, 5.0) }");
    let s = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect("emit");
    assert_eq!(s.ret, ValueTestRet::Float);
    assert!(
        s.fragment.contains("out float _rock_value_out;"),
        "{}",
        s.fragment
    );
    assert!(
        s.fragment.contains("_rock_value_out = t_0();"),
        "{}",
        s.fragment
    );
}

#[test]
fn bool_return_writes_zero_or_one_to_int_output() {
    let (ws, mid) = build_ws("fn t() -> bool { return 3.0 == 3.0 }");
    let s = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect("emit");
    assert_eq!(s.ret, ValueTestRet::Bool);
    assert!(
        s.fragment.contains("out int _rock_value_out;"),
        "{}",
        s.fragment
    );
    assert!(
        s.fragment.contains("_rock_value_out = (t_0() ? 1 : 0);"),
        "{}",
        s.fragment
    );
}

#[test]
fn parametered_root_rejects() {
    let (ws, mid) = build_ws("fn t(x: int) -> int { return x }");
    let e = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect_err("params reject");
    assert!(e.message.contains("no parameters"), "{}", e.message);
}

#[test]
fn non_scalar_return_rejects() {
    let (ws, mid) = build_ws("fn t() -> Vec2 { return vec2(1.0, 2.0) }");
    let e = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect_err("vec reject");
    assert!(e.message.contains("not a scalar"), "{}", e.message);
}

#[test]
fn body_outside_tier0_rejects_with_profile_reason() {
    // A string is well above tier 0 -- the fn library profile pass rejects,
    // surfacing the SKIP reason rather than emitting a guess.
    let (ws, mid) = build_ws("fn t() -> int { let s = \"hi\"; return 1 }");
    let e = emit_value_test(&ws, mid, "t", GlslProfile::Core330).expect_err("string reject");
    assert!(!e.message.is_empty(), "reason present");
}
