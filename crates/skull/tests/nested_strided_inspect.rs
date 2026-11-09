use crate::common::{fn_ref, require_luajit, run_luajit, ws_with_stdlib};
use skull::mir_emit::{emit_full, emit_overlay};

const STRUCT_ROWS: &str = r#"
nobox value Row { a: int, flag: bool, c: int, d: int, e: int }
struct Holder { rows: [Row] }
@nomangle fn inspect_rows() -> string {
    let holder = Holder { rows: [Row { a: 1, flag: false, c: 3, d: 4, e: 5 }] }
    return inspect(holder)
}
"#;

#[test]
fn nested_nobox_struct_rows_reject_materialization_in_full() {
    check_struct_rows(false);
}

#[test]
fn nested_nobox_struct_rows_reject_materialization_in_overlay() {
    check_struct_rows(true);
}

fn check_struct_rows(use_overlay: bool) {
    require_luajit();
    let gc_rows = STRUCT_ROWS
        .replace("c: int", "c: string")
        .replace("c: 3", "c: \"tag\"");
    let empty_rows = STRUCT_ROWS.replace("[Row { a: 1, flag: false, c: 3, d: 4, e: 5 }]", "[]");
    for source in [STRUCT_ROWS, gc_rows.as_str(), empty_rows.as_str()] {
        check_struct_source(source, use_overlay);
    }
}

fn check_struct_source(source: &str, use_overlay: bool) {
    let before = ws_with_stdlib(&source.replace("return inspect(holder)", "return \"before\""));
    let mid = skull::user_module_id(&before).unwrap();
    let (full, mut cache) = emit_full(&before, mid).unwrap();
    let after = ws_with_stdlib(source);
    let f = fn_ref(&after, "inspect_rows");
    let edits = [(f.module, bone_semantic::hir::BodyOwner::Function(f.func))];
    let overlay = emit_overlay(&after, &mut cache, &edits).expect("inspection body overlay");
    let program = if use_overlay {
        format!("{}\n{}", full.lua, overlay.lua)
    } else {
        emit_full(&after, mid).unwrap().0.lua
    };
    let driver = r#"
local ok, message = pcall(inspect_rows)
assert(not ok, "nested struct rows must reject implicit materialization: " .. tostring(message))
assert(tostring(message):find("cannot materialize strided nobox struct", 1, true), tostring(message))
print("rejected")
"#;
    assert_eq!(run_luajit(&format!("{program}\n{driver}")), "rejected",);
}

#[test]
fn nested_strided_enum_rows_still_inspect_payloads_and_units() {
    require_luajit();
    let source = r#"
nobox value Point { a: int, b: int, c: int, d: int }
enum Entry { Empty, Data(Point), Count(int) }
struct Holder { rows: [Entry] }
@nomangle fn inspect_enum_rows() -> string {
    let holder = Holder { rows: [Entry::Data(Point { a: 1, b: 2, c: 3, d: 4 }), Entry::Empty, Entry::Count(9)] }
    return inspect(holder)
}
"#;
    let before = ws_with_stdlib(&source.replace("return inspect(holder)", "return \"before\""));
    let mid = skull::user_module_id(&before).unwrap();
    let (full, mut cache) = emit_full(&before, mid).unwrap();
    let after = ws_with_stdlib(source);
    let f = fn_ref(&after, "inspect_enum_rows");
    let edits = [(f.module, bone_semantic::hir::BodyOwner::Function(f.func))];
    let overlay = emit_overlay(&after, &mut cache, &edits).expect("enum inspection body overlay");
    let driver = r#"
local result = inspect_enum_rows()
assert(result:find("::Data", 1, true), result)
assert(result:find("::Empty", 1, true), result)
assert(result:find("::Count { [1] = 9 }", 1, true), result)
assert(result:find("a = 1", 1, true), result)
assert(result:find("d = 4", 1, true), result)
print("enum rows")
"#;
    for program in [
        emit_full(&after, mid).unwrap().0.lua,
        format!("{}\n{}", full.lua, overlay.lua),
    ] {
        assert_eq!(run_luajit(&format!("{program}\n{driver}")), "enum rows",);
    }
}
