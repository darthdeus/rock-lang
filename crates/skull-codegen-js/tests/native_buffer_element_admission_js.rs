use crate::common::ws_with;

const NATIVEBUFFER_STRING: &str = r#"
fn bad(n: int) -> NativeBuffer<string> { return native_buffer_alloc(n) }
fn main() { let b = bad(2); print("{b.len}") }
"#;

const FLATVEC_STRING: &str = r#"
fn bad(n: int) -> FlatVec<string> { return flat_vec_new(n) }
fn main() { let b = bad(2); print("{b.len}") }
"#;

const GC_VALUE: &str = r#"
value GcValue { text: string }
fn bad(n: int) -> NativeBuffer<GcValue> { return native_buffer_alloc(n) }
fn main() { let b = bad(2); print("{b.len}") }
"#;

fn emit_main(src: &str) -> Result<String, skull_parser::CompilerError> {
    let ws = ws_with(src);
    let mid = *ws.module_ids.get(&ustr::Ustr::from("<test>")).expect("test module");
    let root = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "main")
        .map(|(fid, _)| skull_mir::mir::FuncRef { module: mid, func: fid })
        .expect("main");
    skull_codegen_js::emit::emit_with_closure(&ws, &[(root, "main".to_string())])
}

fn reject(src: &str) -> skull_parser::CompilerError {
    emit_main(src).expect_err("invalid native-buffer element must reject")
}

fn assert_planner_rejection(error: &skull_parser::CompilerError, element: &str) {
    let msg = &error.message;
    assert!(
        msg.contains("requires a valid native-buffer element ctype") && msg.contains(element),
        "{msg}"
    );
    assert!(!error.labels.is_empty(), "native-buffer rejection must be located");
}

#[test]
fn native_buffer_string_rejects_at_the_planner_boundary() {
    let msg = reject(NATIVEBUFFER_STRING);
    assert_planner_rejection(&msg, "string");
}

#[test]
fn flat_vec_string_rejects_through_its_allocation_route() {
    let msg = reject(FLATVEC_STRING);
    assert_planner_rejection(&msg, "string");
}

#[test]
fn gc_carrying_value_rejects_with_its_concrete_name() {
    let msg = reject(GC_VALUE);
    assert_planner_rejection(&msg, "GcValue");
}
