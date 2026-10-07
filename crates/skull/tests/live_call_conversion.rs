use std::process::{Command, Output};

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

const STAGED_FIELD_LEN_CONTROL: &str = r#"
struct Box { values: [int] }

fn apply(strip: Box, insertion: int) -> int {
    let length = strip.values.len()
    let safe = clamp(insertion, 0, length) as int
    return safe
}

fn main() {
    print(apply(Box { values: [7] }, 0))
}
"#;

fn run_pinned_luajit(lua: &str, tag: &str) -> Output {
    let path = std::env::temp_dir().join(format!("skull_{tag}_{}.lua", std::process::id()));
    std::fs::write(&path, format!("{lua}\nmain()\n")).expect("write generated Lua");
    let output = Command::new(rock_host::project::luajit_path())
        .arg(&path)
        .output()
        .expect("spawn repository LuaJIT");
    let _ = std::fs::remove_file(path);
    output
}

#[test]
fn staged_field_len_control_runs() {
    let output = run_pinned_luajit(&emit_module(STAGED_FIELD_LEN_CONTROL), "staged_field_len");
    assert!(
        output.status.success(),
        "the staged field length control must run:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "0");
}

#[test]
fn field_derived_len_is_evaluated_once_through_conversion() {
    let lua = emit_module(FIELD_LEN_IN_CLAMP);
    let body = function_body(&lua, "apply_direct_field");
    let output = run_pinned_luajit(&lua, "field_len_in_clamp");
    assert!(
        output.status.success(),
        "generated Lua must not feed a conversion's repeated producer back into vec_len:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "0\n0");
    assert_eq!(
        body.matches("vec_len_arr_T_spec_p_int(").count(),
        1,
        "the direct field expression must contain one length call:\n{body}"
    );
}
