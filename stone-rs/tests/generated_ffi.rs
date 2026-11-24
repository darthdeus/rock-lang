#[path = "../ffi_codegen.rs"]
#[allow(dead_code)]
mod ffi_codegen;

use ffi_codegen::{Bindings, STONE_NATIVE};

const SOURCES: &[(&str, &str)] = &[
    ("nav::ffi", include_str!("../src/nav/ffi.rs")),
    ("flow_jobs", include_str!("../src/flow_jobs.rs")),
    ("relight_jobs", include_str!("../src/relight_jobs.rs")),
    ("light_table", include_str!("../src/light_table.rs")),
    ("image_data", include_str!("../src/image_data.rs")),
    ("actor_query", include_str!("../src/actor_query.rs")),
    ("meadow", include_str!("../src/meadow.rs")),
];

#[test]
fn generator_owns_all_stone_native_bindings() {
    let result = STONE_NATIVE.bindings(SOURCES.iter().copied()).unwrap();
    assert_eq!(result.cdef.matches(" stone_").count(), 40);
    assert_eq!(result.rock.matches("extern(ffi,").count(), 40);
    assert_eq!(result.rock.matches("fn stone_").count(), 40);
    assert!(include_str!("../../packages/stone-native/stone_native.h").ends_with(&result.cdef));
    assert_eq!(
        include_str!("../../packages/stone-native/bindings.rock"),
        format!("{}{}", ffi_codegen::GENERATED, result.rock)
    );
    assert!(!include_str!("../../packages/stone-native/lib.rock").contains("extern(ffi,"));
}

fn change_binding(module: &str, before: &str, after: &str) -> Result<Bindings, String> {
    let rust = SOURCES
        .iter()
        .find_map(|(path, text)| (*path == module).then_some(*text))
        .unwrap();
    assert!(rust.contains(before), "{module}: {before}");
    let changed = rust.replacen(before, after, 1);
    STONE_NATIVE.bindings(SOURCES.iter().map(|(path, text)| {
        (
            *path,
            if *path == module {
                changed.as_str()
            } else {
                *text
            },
        )
    }))
}

#[test]
fn mismatched_wire_types_leave_stale_c_declarations() {
    let header = include_str!("../../packages/stone-native/stone_native.h");
    for (module, before, after, c) in [
        (
            "flow_jobs",
            "fn stone_flow_poll(handle: u64) -> i32",
            "fn stone_flow_poll(handle: i64) -> i64",
            "int64_t stone_flow_poll(int64_t handle);",
        ),
        (
            "flow_jobs",
            "fn stone_flow_copy(handle: u64, output: *mut f32",
            "fn stone_flow_copy(handle: u64, output: *mut i32",
            "stone_flow_copy(uint64_t handle, int* output,",
        ),
        (
            "meadow",
            "fn stone_meadow_distance(input: *const u8",
            "fn stone_meadow_distance(input: *mut u8",
            "stone_meadow_distance(uint8_t* input,",
        ),
        (
            "light_table",
            "counts: *mut i32",
            "counts: *mut u32",
            "const float* positions, uint32_t* counts",
        ),
        (
            "meadow",
            "fn stone_terrain_noise(x: f64",
            "fn stone_terrain_noise(x: f32",
            "stone_terrain_noise(float x, double y);",
        ),
        (
            "actor_query",
            "include_uninstalled: bool",
            "include_uninstalled: u8",
            "uint8_t include_uninstalled",
        ),
    ] {
        let result = change_binding(module, before, after).unwrap();
        assert!(result.cdef.contains(c), "{c}");
        assert!(!header.ends_with(&result.cdef), "{c}");
    }
}

#[test]
fn a_changed_pointee_leaves_stale_rock_declarations() {
    let generated = change_binding(
        "nav::ffi",
        "fn stone_nav_layout_valid(\n    data: *const NativeNavIntWire",
        "fn stone_nav_layout_valid(\n    data: *const i32",
    )
    .unwrap();
    let want = "fn stone_nav_layout_valid(data: Ptr<int>,";
    assert!(generated.rock.contains(want));
    assert!(!include_str!("../../packages/stone-native/bindings.rock").contains(want));
}

#[test]
fn out_return_requires_a_mutable_pointer_to_a_shared_value() {
    let source = r#"
        /// rock-ffi stone-native return-out
        #[no_mangle]
        pub extern "C" fn example(out: *mut NativeNavIntWire, count: i32) {}
    "#;
    let output = STONE_NATIVE.bindings([("", source)]).unwrap();
    assert!(output
        .cdef
        .contains("void example(NativeNavIntWire* out, int count);"));
    assert!(output
        .rock
        .contains("fn example(count: int) -> NativeNavInt"));
    let source = source.replace("*mut", "*const");
    let error = STONE_NATIVE
        .bindings([("", source.as_str())])
        .err()
        .unwrap();
    assert!(
        error.contains("return-out needs a leading mutable pointer"),
        "{error}"
    );
}

#[test]
fn rejects_a_wider_pointee_with_no_matching_rock_buffer() {
    let error = change_binding(
        "flow_jobs",
        "fn stone_flow_copy(handle: u64, output: *mut f32",
        "fn stone_flow_copy(handle: u64, output: *mut f64",
    )
    .err()
    .unwrap();
    assert!(
        error.contains("stone_flow_copy: output: no Rock pointer mapping for `f64`"),
        "{error}"
    );
}

#[test]
fn changing_a_packed_array_leaves_its_rock_stride_stale() {
    let generated =
        change_binding("meadow", "type Blade = [f32; 7]", "type Blade = [f32; 8]").unwrap();
    let current = include_str!("../../packages/stone-native/bindings.rock");
    assert!(generated.rock.contains("MEADOW_BLADE_BYTES: int = 32"));
    assert!(current.contains("MEADOW_BLADE_BYTES: int = 28"));
}

#[test]
fn a_byte_is_a_rock_integer_without_the_boolean_projection() {
    let generated = change_binding(
        "actor_query",
        "output_len: i64,\n) -> bool",
        "output_len: i64,\n) -> u8",
    )
    .unwrap();
    assert!(generated.cdef.contains("uint8_t stone_actor_query_rect("));
    assert!(generated
        .rock
        .contains("output: Ptr<int>, output_len: int) -> int"));
    assert!(include_str!("../../packages/stone-native/bindings.rock")
        .contains("output: Ptr<int>, output_len: int) -> bool"));
    let generated = STONE_NATIVE
        .bindings([(
            "",
            r#"
        /// rock-ffi stone-native byte-bool
        #[no_mangle]
        pub extern "C" fn example() -> u8 { 0 }
    "#,
        )])
        .unwrap();
    assert!(generated.cdef.contains("uint8_t example(void);"));
    assert!(generated.rock.contains("fn example() -> bool"));
}
