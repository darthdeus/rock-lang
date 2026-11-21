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
local xs = {{[0] = 21}}
for i=1,21 do xs[i]=0 end
local out = collect_private(xs)
assert(out[0] == 3 and out[1] == 0 and out[2] == 7 and out[3] == 14)
assert(xs[21] == 20)
print('ok')
"#
    ));
    assert_eq!(output, "ok");
}

#[test]
fn pushing_an_alias_of_the_receiver_revokes_its_privacy() {
    let lua = emit_module(
        r#"
inline(lua) fn arm_alias(alias: any, xs: [int]) = """
    setmetatable(alias, {__newindex=function(t,k,v)
        xs[0]=0; rawset(t,k,v)
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
local xs={{[0] = 3, 0,0,0}}
local ok, err=pcall(self_item,xs)
assert(not ok and tostring(err):find('out of bounds'))
print('ok')
"#
        )),
        "ok"
    );
}
