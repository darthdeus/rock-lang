use crate::common::{emit_module, function_body};

const SRC: &str = r#"
nobox value Pos { x: int, y: int }
enum Target { None, Tagged(Pos), Named(string) }

fn target(kind: int, x: int, y: int) -> Target {
    if kind == 0 { return Target::None }
    if kind == 1 { return Target::Tagged(Pos { x: x, y: y }) }
    return Target::Named("other")
}

@nomangle fn equal_tag(kind: int, x: int, y: int) -> bool {
    return target(kind, x, y) == Target::Tagged(Pos { x: 3, y: 4 })
}

@nomangle fn not_equal_tag(kind: int, x: int, y: int) -> bool {
    return target(kind, x, y) != Target::Tagged(Pos { x: 3, y: 4 })
}

@nomangle fn equal_targets(a: int, ax: int, ay: int, b: int, bx: int, by: int) -> bool {
    return target(a, ax, ay) == target(b, bx, by)
}

fn maybe_target(kind: int, x: int, y: int) -> Target? {
    if kind < 0 { return null }
    return target(kind, x, y)
}

@nomangle fn equal_maybe(a: int, b: int) -> bool {
    return maybe_target(a, 3, 4) == maybe_target(b, 3, 4)
}
"#;

#[test]
fn flat_enum_equality_compares_threaded_cells_without_boxes() {
    let lua = emit_module(SRC);
    for name in ["equal_tag", "not_equal_tag", "equal_targets", "equal_maybe"] {
        let body = function_body(&lua, name);
        assert!(
            !body.contains("{__variant = "),
            "`{name}` materialized an enum:\n{body}"
        );
        assert!(
            !body.contains("__op_eq") && !body.contains("__op_ne"),
            "`{name}` called structural equality:\n{body}"
        );
    }
}

#[test]
fn flat_enum_equality_preserves_variant_and_payload_semantics() {
    if std::process::Command::new("luajit")
        .arg("-v")
        .output()
        .is_err()
    {
        eprintln!("luajit not found; skipping execution check");
        return;
    }
    let mut lua = emit_module(SRC);
    lua.push_str("\nprint(equal_tag(1, 3, 4), equal_tag(1, 3, 5), equal_tag(0, 3, 4), equal_tag(2, 3, 4), not_equal_tag(1, 3, 4), not_equal_tag(1, 3, 5), equal_targets(1, 3, 4, 1, 3, 4), equal_targets(1, 3, 4, 1, 3, 5), equal_targets(0, 0, 0, 0, 9, 9), equal_targets(0, 0, 0, 2, 0, 0), equal_maybe(-1, -1), equal_maybe(-1, 1), equal_maybe(1, 1))\n");
    let path = std::env::temp_dir().join(format!("skull_flat_enum_eq_{}.lua", std::process::id()));
    std::fs::write(&path, lua).expect("write Lua fixture");
    let out = std::process::Command::new("luajit")
        .arg(&path)
        .output()
        .expect("run Lua fixture");
    let _ = std::fs::remove_file(path);
    assert!(
        out.status.success(),
        "LuaJIT failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "true\tfalse\tfalse\tfalse\tfalse\ttrue\ttrue\tfalse\ttrue\tfalse\ttrue\tfalse\ttrue"
    );
}
