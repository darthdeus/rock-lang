//! A nullable `value` read out of a field and consumed at once (`if let`,
//! `unwrap()`) must not rebuild the value's box. The read copies the stored
//! box on its present arm and joins the copy with `nil` in one cell; SROA
//! used to keep that cell whole because a nil test is not a field read, so
//! Lua allocated one table per nested value on every read. MAG's live mob
//! poll (`if let pc = mob.attack_co`) paid two tables per mob per tick.

use crate::common::{emit_module, function_body, run_luajit};

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

fn held(h: Holder) -> bool { return h.t != null }

fn first_a(h: Holder) -> int {
    if h.t == null { return -1 }
    return h.t.unwrap().st.a + h.t.unwrap().st.b
}

@nomangle inline(lua) fn __out(s: string) = "io.write(s, ' ')"

fn main() {
    let h = Holder { t: Tk { co: "x", st: Ent { a: 1, b: 2 } } }
    for _ in 0..3 { poll_in(h) }
    __out("{h.polls} {held(h)} {first_a(h)}")
    h.t = Tk { co: "y", st: Ent { a: 0, b: 5 } }
    poll_in(h)
    poll_in(h)
    __out("{h.polls} {!held(h)} {first_a(h)}")
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
    assert_eq!(out, "3 true 3 4 true -1");
}

#[test]
fn unwrap_of_nullable_value_field_builds_no_box() {
    let lua = emit_module(JOIN);
    let body = function_body(&lua, "first_a");
    for ctor in ["Tk_vt(", "Tk_ct(", "Ent_vt(", "Ent_ct("] {
        assert!(!body.contains(ctor), "first_a rebuilds `{ctor}` to unwrap a field:\n{body}");
    }
}
