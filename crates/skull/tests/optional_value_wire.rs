//! An ORDINARY cdata-tier `value?` crosses a Rock-to-Rock Lua call as a
//! presence lane plus its payload leaves, the wire a `nobox` `T?` already
//! takes (a GC-tier `T?`, `Tag` below, keeps its boxed slot). A consumer
//! that only needs presence (`??`, `== null`) reads the lane, so the
//! non-escaping path builds no box; an escape (a field, a boxed local) boxes
//! once, and to `nil` when absent.

use crate::common::{emit_module, function_body, run_luajit};

const OPTIONAL_VALUE: &str = r#"
value Mix { gain: float, pan: float }
value V2 { x: float, y: float }
value Outer { m: Mix, w: float }
value Tag { name: string, n: float }

struct Holder { mix: Mix? }

fn mk_v(a: float) -> V2? {
    if a < 0.0 { return null }
    return V2 { x: a, y: a + 1.0 }
}

fn mk_mix(a: float) -> Mix? {
    let v = mk_v(a) ?? return null
    return Mix { gain: v.x, pan: v.y }
}

fn mk_outer(a: float) -> Outer? {
    let m = mk_mix(a) ?? return null
    return Outer { m: m, w: 3.0 }
}

fn mk_tag(a: float) -> Tag? {
    if a < 0.0 { return null }
    return Tag { name: "t", n: a }
}

fn play(a: float) -> float {
    let mix = mk_mix(a) ?? return -1.0
    return mix.gain + mix.pan
}

fn use_mix(m: Mix?) -> float {
    if m != null { return m.gain }
    return 0.0
}

fn stored(h: Holder) -> Mix? {
    return h.mix
}

fn main() {
    print(play(2.0))
    print(play(-1.0))
    let m = mk_mix(2.0)
    if m != null {
        print(m.gain + m.pan)
    }
    print(mk_mix(-1.0) == null)
    print(mk_mix(1.0) != null)
    let h = Holder { mix: mk_mix(4.0) }
    let h2 = Holder { mix: mk_mix(-4.0) }
    print(h.mix != null)
    print(h2.mix == null)
    print(use_mix(mk_mix(5.0)))
    print(use_mix(mk_mix(-5.0)))
    let o = mk_outer(1.0) ?? Outer { m: Mix { gain: 0.0, pan: 0.0 }, w: 0.0 }
    print(o.m.gain + o.w)
    let o2 = mk_outer(-1.0) ?? Outer { m: Mix { gain: 7.0, pan: 0.0 }, w: 0.0 }
    print(o2.m.gain)
    let d = mk_mix(-3.0) ?? Mix { gain: 9.0, pan: 9.0 }
    print(d.gain)
    print(stored(h) != null)
    print(stored(h2) == null)
    print((stored(h2) != null) == false)
    print(use_mix(stored(h)))
    let t = mk_tag(2.0) ?? Tag { name: "d", n: 0.0 }
    print(t.name)
    let t2 = mk_tag(-2.0) ?? Tag { name: "d", n: 0.0 }
    print(t2.name)
}
"#;

#[test]
fn optional_value_return_builds_no_box_on_the_presence_path() {
    let lua = emit_module(OPTIONAL_VALUE);
    for (name, ctor) in [
        ("mk_mix", "Mix_ct("),
        ("mk_mix", "V2_ct("),
        ("mk_outer", "Outer_ct("),
        ("mk_outer", "Mix_ct("),
        ("play", "Mix_ct("),
    ] {
        let body = function_body(&lua, name);
        assert!(!body.contains(ctor), "`{name}` builds `{ctor}..)`:\n{body}");
    }
    let body = function_body(&lua, "mk_mix");
    assert!(
        body.contains("do return (true), "),
        "present arm is the presence lane:\n{body}"
    );
    assert!(
        body.contains("do return (false), "),
        "absent arm is the presence lane:\n{body}"
    );
}

#[test]
fn optional_value_wire_keeps_null_present_nested_and_escape_semantics() {
    let lua = emit_module(OPTIONAL_VALUE);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "optional_value_wire"),
        "5\n-1\n5\ntrue\ntrue\ntrue\ntrue\n5\n0\n4\n7\n9\ntrue\ntrue\ntrue\n4\nt\nd"
    );
}
