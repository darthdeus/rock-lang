//! The JS deepcopy runtime (`JS_DEEPCOPY_RUNTIME`) on shapes only the JS
//! backend has: a code-ref environment and a NativeBuffer with typed leaf views.
//! Deepcopy of program values is tests/library/js_deepcopy.rock.

#[test]
fn copied_code_ref_env_shares_logical_slot() {
    if !crate::common::require_node() {
        return;
    }
    let program = format!(
        "{}{}\n\
         function old(e) {{ return e.c + 1; }}\n\
         function edited(e) {{ return e.c + 100; }}\n\
         __rock_code_slot_begin();\n\
         __rock_code_slot_stage_one(\"probe\", old, \"probe-abi\");\n\
         __rock_code_slot_publish();\n\
         var original = {{ __slot: __rock_code_slot_ref(\"probe\"), c: 7 }};\n\
         var copy = deepcopy(original);\n\
         __rock_code_slot_begin();\n\
         __rock_code_slot_stage_one(\"probe\", edited, \"probe-abi\");\n\
         __rock_code_slot_publish();\n\
         console.log(__rock_call(copy));",
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_DEEPCOPY_RUNTIME,
    );
    let out = crate::common::run_node(&program);
    assert_eq!(out.trim(), "107", "{program}");
}

// A value-struct NativeBuffer is one flat array plus typed leaf views over its
// bytes. The copy's views must alias the copy's own array, and the array keeps
// its element width, so a write through a view shows through `.ptr`.
#[test]
fn native_buffer_copy_rebuilds_leaf_views_over_its_storage() {
    if !crate::common::require_node() {
        return;
    }
    let program = format!(
        "{}{}\n\
         var ptr = Object.assign(new Float32Array(4), {{__rock_elem_size: 8}});\n\
         var nb = {{ ptr: ptr, len: 2, __view_int: new Int32Array(ptr.buffer, ptr.byteOffset) }};\n\
         nb.__view_int[0] = 3;\n\
         var copy = deepcopy({{ buf: nb, len: 1 }});\n\
         copy.buf.__view_int[0] = 9;\n\
         var shared = new Int32Array(copy.buf.ptr.buffer)[0];\n\
         console.log([nb.__view_int[0], shared, copy.buf.ptr.__rock_elem_size,\n\
             copy.buf.ptr.buffer !== ptr.buffer].join(\",\"));",
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_DEEPCOPY_RUNTIME,
    );
    let out = crate::common::run_node(&program);
    assert_eq!(out.trim(), "3,9,8,true", "{program}");
}
