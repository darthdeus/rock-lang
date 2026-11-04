use crate::common::{emit_module, require_luajit, run_luajit};

// A flat enum's null is its nil tag. Each case returns its text from a helper,
// so a formatter that falls off its variant tests returns no string instead of
// the text "nil".
const SRC: &str = r#"
nobox value Pos { x: int, y: int }
enum Target { Idle, Tagged(Pos) }

fn absent_text(t: Target?) -> string { return tostring(t) }
fn absent_interp(t: Target?) -> string { return "{t}" }

struct Holder { target: Target? }

@nomangle fn null_text() -> bool { return absent_text(null) == "nil" }
@nomangle fn null_interp() -> bool { return absent_interp(null) == "nil" }
@nomangle fn stored_null_text() -> bool {
    let h = Holder { target: null }
    return absent_text(h.target) == "nil"
}
@nomangle fn present_text() -> string { return absent_text(Target::Tagged(Pos { x: 3, y: 4 })) }
"#;

#[test]
fn nullable_flat_enum_formats_null_as_nil() {
    require_luajit();
    let mut lua = emit_module(SRC);
    lua.push_str(
        "\nprint(null_text(), null_interp(), stored_null_text())\nprint(present_text())\n",
    );
    let out = run_luajit(&lua, "flat_enum_null_format");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines,
        ["true\ttrue\ttrue", "Target::Tagged(Pos { x: 3, y: 4 })"],
        "{lua}"
    );
}
