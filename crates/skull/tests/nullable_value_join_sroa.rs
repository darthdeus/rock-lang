//! A nullable `value` read out of a field and consumed at once (`if let`,
//! `unwrap()`) must not rebuild the value's box. The read copies the stored
//! box on its present arm and joins the copy with `nil` in one cell; SROA
//! used to keep that cell whole because a nil test is not a field read, so
//! Lua allocated one table per nested value on every read. application's live mob
//! poll (`if let pc = mob.attack_co`) paid two tables per mob per tick.

use crate::common::{emit_module, function_body, run_luajit};

const PIPELINE: &str = r#"
nobox value Pipeline {
    shader_name: string,
    parametric_quads: bool = false,
    after_scene: bool = false,
}
"#;

#[test]
fn nullable_pipeline_transfers_do_not_materialize() {
    let fixture = r#"
struct RecipeHolder { pipeline: Pipeline? = null }
fn publish(h: RecipeHolder, p: Pipeline) { h.pipeline = p }
fn copy_recipe(dst: RecipeHolder, src: RecipeHolder) { dst.pipeline = src.pipeline }
fn clear_recipe(h: RecipeHolder) { h.pipeline = null }
fn cached_pipeline() -> Pipeline {
    static p = materialize(Pipeline { shader_name: "ui" })
    p
}
@nomangle inline(lua) fn __out(s: string) = "io.write(s, ' ')"
fn describe(h: RecipeHolder) {
    if let p = h.pipeline {
        __out("{p.shader_name} {p.parametric_quads} {p.after_scene}")
    } else { __out("empty") }
}
fn main() {
    let src = RecipeHolder {}
    let dst = RecipeHolder {}
    publish(src, Pipeline { shader_name: "world", parametric_quads: true, after_scene: true })
    copy_recipe(dst, src)
    clear_recipe(src)
    describe(src)
    describe(dst)
    copy_recipe(dst, src)
    describe(dst)
    publish(src, cached_pipeline())
    copy_recipe(dst, src)
    describe(dst)
}
"#;
    let lua = emit_module(&format!("{PIPELINE}\n{fixture}"));
    for name in ["publish", "copy_recipe"] {
        let body = function_body(&lua, name);
        for ctor in ["Pipeline_vt(", "Pipeline_ct("] {
            assert!(!body.contains(ctor), "{name} materializes `{ctor}`:\n{body}");
        }
    }
    assert_eq!(run_luajit(&format!("{lua}\nmain()\n"), "pipeline_nullable_transfers"),
        "empty world true true empty ui false false");
}

const JOIN: &str = r#"
value Ent { a: int, b: int }
value Tk { co: string, st: Ent }
struct Holder { t: Tk? = null, polls: int = 0 }

fn use_tk(t: Tk) -> bool { return t.st.a > 0 }

fn poll_in(h: Holder) {
    if let t = h.t {
        h.polls += 1
        if !use_tk(t) { h.t = null }
    }
}

fn reads_st(h: Holder, xs: [int]) -> bool {
    let t = h.t ?? return false
    for x in xs {
        if t.st.a == x { return true }
    }
    return false
}

fn first_match(h: Holder, xs: [int]) -> Tk? {
    let t = h.t ?? return null
    for x in xs {
        if t.st.a == x { return t }
    }
    return t
}

fn held(h: Holder) -> bool { return h.t != null }

fn first_a(h: Holder) -> int {
    if h.t == null { return -1 }
    return h.t.unwrap().st.a + h.t.unwrap().st.b
}

@nomangle inline(lua) fn __out(s: string) = "io.write(s, ' ')"

fn main() {
    let h = Holder { t: Tk { co: "x", st: Ent { a: 1, b: 2 } } }
    for _ in 0..3 { poll_in(h) }
    __out("{h.polls} {held(h)} {first_a(h)} {reads_st(h, [0, 1])} {reads_st(h, [2])}")
    h.t = Tk { co: "y", st: Ent { a: 0, b: 5 } }
    poll_in(h)
    poll_in(h)
    __out("{h.polls} {!held(h)} {first_a(h)} {reads_st(h, [0])}")
}
"#;

#[test]
fn if_let_join_of_nullable_value_field_builds_no_box() {
    let lua = emit_module(JOIN);
    let body = function_body(&lua, "poll_in");
    for ctor in ["Tk_vt(", "Tk_ct(", "Ent_vt(", "Ent_ct("] {
        assert!(!body.contains(ctor), "poll_in rebuilds `{ctor}` for its `if let` join:\n{body}");
    }
    let out = run_luajit(&format!("{lua}\nmain()\n"), "nullable_value_join");
    assert_eq!(out, "3 true 3 true false 4 true -1 false");
}

#[test]
fn unwrap_of_nullable_value_field_builds_no_box() {
    let lua = emit_module(JOIN);
    let body = function_body(&lua, "first_a");
    for ctor in ["Tk_vt(", "Tk_ct(", "Ent_vt(", "Ent_ct("] {
        assert!(!body.contains(ctor), "first_a rebuilds `{ctor}` to unwrap a field:\n{body}");
    }
}

#[test]
fn coalesce_return_binding_of_nullable_value_field_builds_no_box() {
    let lua = emit_module(JOIN);
    let body = function_body(&lua, "reads_st");
    for ctor in ["Tk_vt(", "Tk_ct(", "Ent_vt(", "Ent_ct("] {
        assert!(!body.contains(ctor), "reads_st rebuilds `{ctor}` for its `??` binding:\n{body}");
    }
}


// GC-tier nullable returns carry presence and leaves, so both
// escapes must keep the coalesce binding unboxed, including its nested value.
#[test]
fn escaping_coalesce_binding_returns_leaves_without_copying() {
    let checks = r#"
@nomangle fn check_nullable_escape() {
    let h = Holder { t: Tk { co: "held", st: Ent { a: 7, b: 9 } } }
    let hit = first_match(h, [7]).unwrap()
    let miss = first_match(h, [0]).unwrap()
    __out("{hit.co} {hit.st.a} {hit.st.b} {miss == hit}")
    h.t = null
    __out("{first_match(h, [7]) == null}")
}
"#;
    let lua = emit_module(&format!("{JOIN}\n{checks}"));
    let body = function_body(&lua, "first_match");
    for ctor in ["Tk_vt(", "Tk_ct(", "Ent_vt(", "Ent_ct("] {
        assert!(!body.contains(ctor), "first_match rebuilds `{ctor}` at a nullable return:\n{body}");
    }
    assert!(!body.contains('{'), "first_match builds a raw value box:\n{body}");
    assert_eq!(run_luajit(&format!("{lua}\ncheck_nullable_escape()\n"), "escaping_nullable_value_join"), "held 7 9 true true");
}
