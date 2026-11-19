use crate::common::{emit_module, function_body, run_luajit};

const COLLECT: &str = r#"
@nomangle fn collect_private(xs: [int]) -> [int] {
    let out: [int] = []
    for i in 0..xs.len() {
        xs[i] = i
        if i % 7 == 0 { out.push(i) }
    }
    return out
}
"#;

#[test]
fn a_private_output_push_preserves_input_bounds() {
    let lua = emit_module(COLLECT);
    let body = function_body(&lua, "collect_private");
    assert!(!body.contains("__vec_checked_set"));
    let output = run_luajit(&format!(
        r#"{lua}
local xs = {{_data = {{}}, len = 21}}
for i=1,21 do xs._data[i]=0 end
local out = collect_private(xs)
assert(out.len == 3 and out._data[1] == 0 and out._data[2] == 7 and out._data[3] == 14)
assert(xs._data[21] == 20)
print('ok')
"#
    ));
    assert_eq!(output.trim(), "ok");
}

#[test]
fn local_aliases_keep_private_vector_provenance() {
    let source = COLLECT.replace(
        "if i % 7 == 0 { out.push(i) }",
        "let alias = out\n        if i % 7 == 0 { alias.push(i) }",
    );
    let lua = emit_module(&source);
    assert!(!function_body(&lua, "collect_private").contains("__vec_checked_set"));
}

#[test]
fn exposed_vector_backings_keep_callback_bounds_checks() {
    for setup in [
        "arm(out, xs)",
        "let alias = out\narm(alias, xs)",
        "let holder = Holder { items: out }\narm(holder.items, xs)",
        "let holder = Holder { items: [] }\nholder.items = out\narm(holder.items, xs)",
        "saved = out\narm(saved, xs)",
        "let holders: [[int]] = [out]\narm(holders[0], xs)",
        "let holders: [[int]] = []\nholders.push(out)\narm(holders[0], xs)",
        "let holders: [[int]] = [[]]\nholders[0] = out\narm(holders[0], xs)",
        "let expose = () => arm(out, xs)\nexpose()",
        "if xs.len() > 0 { arm(out, xs) }",
    ] {
        let source = format!(
            r#"
struct Holder {{ items: [int] }}
global saved: [int]? = null
inline(lua) fn arm(out: [int], xs: [int]) = """
    setmetatable(out._data, {{__newindex = function(t, k, v)
        _G.__private_vec_hits = (_G.__private_vec_hits or 0) + 1
        xs.len = 0
        rawset(t, k, v)
    end}})
"""
@nomangle fn exposed(xs: [int]) -> int {{
    let out: [int] = []
    {setup}
    for i in 0..xs.len() {{
        xs[i] = i
        out.push(i)
    }}
    return out.len()
}}
"#
        );
        let lua = emit_module(&source);
        assert!(function_body(&lua, "exposed").contains("__vec_checked_set"));
        let output = run_luajit(&format!(
            r#"{lua}
local xs = {{len = 3, _data = {{0, 0, 0}}}}
local ok, err = pcall(exposed, xs)
assert(not ok and tostring(err):find('out of bounds'))
assert(__private_vec_hits == 1)
print('ok')
"#
        ));
        assert_eq!(output.trim(), "ok");
    }
}

#[test]
fn foreign_reassignment_and_join_do_not_inherit_freshness() {
    for setup in [
        "let out: [int] = []\nout = foreign",
        "let out: [int] = if choose { [] } else { foreign }",
    ] {
        let lua = emit_module(&format!(
            r#"
@nomangle fn choose_output(xs: [int], foreign: [int], choose: bool) -> int {{
    {setup}
    for i in 0..xs.len() {{ xs[i] = i; out.push(i) }}
    return out.len()
}}
"#
        ));
        assert!(function_body(&lua, "choose_output").contains("__vec_checked_set"));
        let output = run_luajit(&format!(
            r#"{lua}
local xs = {{len=3, _data={{0,0,0}}}}
local foreign = {{len=0, _data=setmetatable({{}}, {{__newindex=function(t,k,v)
    xs.len=0; rawset(t,k,v)
end}})}}
local ok, err = pcall(choose_output, xs, foreign, false)
assert(not ok and tostring(err):find('out of bounds'))
print('ok')
"#
        ));
        assert_eq!(output.trim(), "ok");
    }
}

#[test]
fn a_reexecuted_allocation_does_not_reprivatize_an_old_escaped_alias() {
    let lua = emit_module(
        r#"
inline(lua) fn arm(out: [int], xs: [int]) = """
    setmetatable(out._data, {__newindex=function(t,k,v)
        xs.len=0; rawset(t,k,v)
    end})
"""
@nomangle fn generations(xs: [int]) -> int {
    let saved: [int]? = null
    let sum = 0
    for generation in 0..2 {
        let fresh: [int] = []
        if generation == 0 {
            saved = fresh
            arm(fresh, xs)
        } else {
            let old = saved ?? []
            for i in 0..xs.len() { xs[i] = i; old.push(i) }
        }
        sum += fresh.len()
    }
    return sum
}
"#,
    );
    assert!(function_body(&lua, "generations").contains("__vec_checked_set"));
    assert_eq!(
        run_luajit(&format!(
            r#"{lua}
local xs={{len=3, _data={{0,0,0}}}}
local ok, err=pcall(generations,xs)
assert(not ok and tostring(err):find('out of bounds'))
print('ok')
"#
        )),
        "ok"
    );
}

#[test]
fn pushing_an_alias_of_the_receiver_revokes_its_privacy() {
    let lua = emit_module(
        r#"
inline(lua) fn arm_alias(alias: any, xs: [int]) = """
    setmetatable(alias._data, {__newindex=function(t,k,v)
        xs.len=0; rawset(t,k,v)
    end})
"""
@nomangle fn self_item(xs: [int]) -> int {
    let out: [any] = []
    out.push(out)
    arm_alias(out[0], xs)
    for i in 0..xs.len() { xs[i] = i; out.push(i) }
    return out.len()
}
"#,
    );
    assert!(function_body(&lua, "self_item").contains("__vec_checked_set"));
    assert_eq!(
        run_luajit(&format!(
            r#"{lua}
local xs={{len=3, _data={{0,0,0}}}}
local ok, err=pcall(self_item,xs)
assert(not ok and tostring(err):find('out of bounds'))
print('ok')
"#
        )),
        "ok"
    );
}

#[test]
fn an_opaque_call_without_a_vector_reference_does_not_publish_it() {
    let source = format!(
        "{}\n{}",
        r#"inline(lua) fn ping() = "_G.__private_ping = 1""#,
        COLLECT.replace("let out: [int] = []", "let out: [int] = []\n    ping()")
    );
    let lua = emit_module(&source);
    assert!(!function_body(&lua, "collect_private").contains("__vec_checked_set"));
}

#[test]
fn the_explicit_vec_push_intrinsic_uses_the_same_private_proof() {
    let lua = emit_module(&COLLECT.replace("out.push(i)", "vec_push(out, i)"));
    assert!(!function_body(&lua, "collect_private").contains("__vec_checked_set"));
}
