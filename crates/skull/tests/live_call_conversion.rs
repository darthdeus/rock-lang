use std::process::Command;

use crate::common::{emit_module, function_body};

const FIELD_LEN_IN_CLAMP: &str = r#"
struct Box { values: [int] }

fn apply_temp_field(strip: Box, insertion: int) -> int {
    let length = strip.values.len()
    let safe = clamp(insertion, 0, length) as int
    return safe
}

fn apply_direct_field(strip: Box, insertion: int) -> int {
    let safe = clamp(insertion, 0, strip.values.len()) as int
    return safe
}

fn main() {
    let strip = Box { values: [7] }
    print(apply_temp_field(strip, 0))
    print(apply_direct_field(strip, 0))
}
"#;

fn run_main_on_pinned_luajit(lua: &str) -> String {
    let path = std::env::temp_dir().join(format!("skull_field_len_{}.lua", std::process::id()));
    std::fs::write(&path, format!("{lua}\nmain()\n")).expect("write generated Lua");
    let output = Command::new(rock_host::project::luajit_path())
        .arg(&path)
        .output()
        .expect("spawn repository LuaJIT");
    let _ = std::fs::remove_file(path);
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .replace("\r\n", "\n")
}

#[test]
fn field_derived_len_is_evaluated_once_through_conversion() {
    let lua = emit_module(FIELD_LEN_IN_CLAMP);
    let body = function_body(&lua, "apply_direct_field");
    assert_eq!(run_main_on_pinned_luajit(&lua), "0\n0");
    let length_calls = body.matches("len_arr_T_spec_p_int(").count();
    let length_reads = body.matches(".len").count();
    assert_eq!(length_calls + length_reads, 1);
}
