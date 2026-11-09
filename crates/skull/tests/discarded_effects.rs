use crate::common::{run_luajit, ws_with_stdlib};
use skull::mir_emit::emit_full;

#[test]
fn unused_throwing_conversion_preserves_runtime_error() {
    let ws = ws_with_stdlib(
        r#"
@nomangle fn probe(n: int) -> int { let unused = n as uint; return 7 }
"#,
    );
    let lua = emit_full(&ws, skull::user_module_id(&ws).unwrap())
        .unwrap()
        .0
        .lua;
    let program = format!("{lua}\nlocal ok = pcall(probe, -1); print(ok, probe(1))\n");
    assert_eq!(run_luajit(&program), "false\t7");
}

#[test]
fn aggregate_return_materialization_does_not_project_placeholder_nil() {
    let ws = ws_with_stdlib(
        r#"
fn normalize_optional_value(x: float, y: float) -> Vec2? {
    let delta = v(x, y)
    if delta.length() <= 0.001 { return null }
    return delta.normalize()
}
fn normalize_tail_value(x: float, y: float) -> Vec2? { v(x, y).normalize() }
@nomangle fn normalize_optional_probe(x: float, y: float) -> float {
    let p = normalize_optional_value(x, y) ?? return -1
    return p.x + p.y
}
@nomangle fn normalize_tail_probe(x: float, y: float) -> float {
    let p = normalize_tail_value(x, y) ?? return -1
    p.x + p.y
}
"#,
    );
    let lua = emit_full(&ws, skull::user_module_id(&ws).unwrap())
        .unwrap()
        .0
        .lua;
    let program = format!(
        "{lua}\nprint(normalize_optional_probe(3, 4), normalize_optional_probe(0, 0), normalize_tail_probe(3, 4), normalize_tail_probe(0, 0))\n"
    );
    assert_eq!(run_luajit(&program), "1.4\t-1\t1.4\t0");
}
