use crate::common::{body_id, edit, run_luajit, ws_with_stdlib};

const STRUCT_ROWS: &str = r#"
nobox value Row { a: int, flag: bool, c: int, d: int, e: int }
struct Holder { rows: [Row] }
@nomangle fn inspect_rows() -> string {
    let holder = Holder { rows: [Row { a: 1, flag: false, c: 3, d: 4, e: 5 }] }
    return inspect(holder)
}
"#;

fn full_and_overlay_programs(source: &str, name: &str) -> [String; 2] {
    let before = ws_with_stdlib(&source.replace("return inspect(holder)", "return \"before\""));
    let after = ws_with_stdlib(source);
    let edit = edit(&before, &after, &[body_id(&after, name)]);
    let overlay = edit.overlay.expect("inspection body overlay");
    [
        edit.incremental.lua,
        format!("{}\n{}", edit.base.lua, overlay.lua),
    ]
}

#[test]
fn nested_nobox_struct_rows_reject_materialization() {
    let gc_rows = STRUCT_ROWS
        .replace("c: int", "c: string")
        .replace("c: 3", "c: \"tag\"");
    let empty_rows = STRUCT_ROWS.replace("[Row { a: 1, flag: false, c: 3, d: 4, e: 5 }]", "[]");
    let driver = r#"
local ok, message = pcall(inspect_rows)
assert(not ok, "nested struct rows must reject implicit materialization: " .. tostring(message))
assert(tostring(message):find("cannot materialize strided nobox struct", 1, true), tostring(message))
print("rejected")
"#;
    for source in [STRUCT_ROWS, gc_rows.as_str(), empty_rows.as_str()] {
        for program in full_and_overlay_programs(source, "inspect_rows") {
            assert_eq!(run_luajit(&format!("{program}\n{driver}")), "rejected");
        }
    }
}

#[test]
fn nested_strided_enum_rows_still_inspect_payloads_and_units() {
    let source = r#"
nobox value Point { a: int, b: int, c: int, d: int }
enum Entry { Empty, Data(Point), Count(int) }
struct Holder { rows: [Entry] }
@nomangle fn inspect_enum_rows() -> string {
    let holder = Holder { rows: [Entry::Data(Point { a: 1, b: 2, c: 3, d: 4 }), Entry::Empty, Entry::Count(9)] }
    return inspect(holder)
}
"#;
    let driver = r#"
local result = inspect_enum_rows()
assert(result:find("::Data", 1, true), result)
assert(result:find("::Empty", 1, true), result)
assert(result:find("::Count { [1] = 9 }", 1, true), result)
assert(result:find("a = 1", 1, true), result)
assert(result:find("d = 4", 1, true), result)
print("enum rows")
"#;
    for program in full_and_overlay_programs(source, "inspect_enum_rows") {
        assert_eq!(run_luajit(&format!("{program}\n{driver}")), "enum rows");
    }
}
