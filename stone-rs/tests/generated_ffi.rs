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

#[test]
fn engine_stats_prototypes_are_generated() {
    let header = include_str!("../../packages/stone/engine/runtime_ffi.h");
    let (_, generated) = header
        .split_once(ffi_codegen::GENERATED)
        .expect("generated engine declarations");
    for name in [
        "stone_get_graphics_stats",
        "stone_get_image_frame_stats",
        "stone_get_image_profile_stats",
    ] {
        assert!(generated.contains(name), "{name}");
    }
}

#[test]
fn graphics_stats_requires_a_writable_result_slot() {
    let rust = include_str!("../src/lib.rs");
    assert!(rust.contains("fn stone_get_graphics_stats(stats: *mut GraphicsStats)"));
    let changed = rust.replace(
        "fn stone_get_graphics_stats(stats: *mut GraphicsStats)",
        "fn stone_get_graphics_stats(stats: *const GraphicsStats)",
    );
    let problem = ffi_codegen::runtime::STONE_ENGINE
        .bindings([("", changed.as_str())])
        .err()
        .unwrap();
    assert!(
        problem.contains("stone_get_graphics_stats: return-out needs a leading mutable pointer")
    );
}

#[test]
fn stone_event_layout_comes_from_the_c_header() {
    let generated = include_str!(concat!(env!("OUT_DIR"), "/stone/ffi.rs"));
    assert!(generated.contains("pub struct StoneEvent {"));
}

#[test]
fn stone_event_text_extent_follows_the_c_layout() {
    let header = include_str!("../../packages/stone/engine/runtime_ffi.h");
    assert!(header.contains("char text[32];"));
    let changed = header.replace("char text[32];", "char text[48];");
    let generate = ffi_codegen::runtime::STONE_ENGINE.c_owned_layout.unwrap();
    let generated = generate(&changed).unwrap();
    assert!(generated.contains("const STONE_TEXT_INPUT_BYTES: usize = 48;"));
}

#[test]
fn engine_projections_preserve_the_rust_wire_types() {
    for (options, signature, c, rock, mismatch, error) in [
        (
            "",
            "(text: *const c_char)",
            "void example(const char* text);",
            "pub extern(ffi, \"stone\") fn example(text: string)",
            "*mut c_char",
            "no Rock pointer mapping",
        ),
        (
            "nullable:text",
            "(text: *const c_char)",
            "void example(const char* text);",
            "pub extern(ffi, \"stone\") fn example(text: string?)",
            "*const u8",
            "nullable requires a const C string",
        ),
        (
            "c-bool",
            "() -> bool",
            "bool example(void);",
            "pub extern(ffi, \"stone\") fn example() -> bool",
            "u8",
            "c-bool requires a bool return",
        ),
        (
            "byte-bool",
            "() -> u8",
            "uint8_t example(void);",
            "pub extern(ffi, \"stone\") fn example() -> bool",
            "i32",
            "byte-bool requires a u8 return",
        ),
        (
            "",
            "(size: usize)",
            "void example(size_t size);",
            "pub extern(ffi, \"stone\") fn example(size: int)",
            "u64",
            "uint64_t size",
        ),
        (
            "",
            "(window: *mut StoneWindow) -> *mut StoneWindow",
            "StoneWindow* example(StoneWindow* window);",
            "pub extern(ffi, \"stone\") fn example(window: StoneWindow) -> StoneWindow",
            "*mut c_int",
            "int* window",
        ),
        (
            "uint:handle uint:return",
            "(handle: u64) -> u64",
            "uint64_t example(uint64_t handle);",
            "pub extern(ffi, \"stone\") fn example(handle: uint) -> uint",
            "i64",
            "uint requires an unsigned integer",
        ),
        (
            "",
            "(width: *mut c_int, size: *mut usize)",
            "void example(int* width, size_t* size);",
            "pub extern(ffi, \"stone\") fn example(width: Ptr<int>, size: Ptr<int>)",
            "*const c_int",
            "const int* width",
        ),
        (
            "",
            "(bytes: *const u8)",
            "void example(const uint8_t* bytes);",
            "pub extern(ffi, \"stone\") fn example(bytes: PtrU8)",
            "*const i32",
            "const int* bytes",
        ),
        (
            "",
            "(samples: *mut c_float)",
            "void example(float* samples);",
            "pub extern(ffi, \"stone\") fn example(samples: Ptr<float>)",
            "*mut c_double",
            "no Rock pointer mapping",
        ),
    ] {
        let source = |signature: &str| {
            format!(
                "/// rock-ffi stone {options}\n#[no_mangle]\npub extern \"C\" fn example{signature} {{}}"
            )
        };
        let original = source(signature);
        let generated = ffi_codegen::runtime::STONE_ENGINE
            .bindings([("", original.as_str())])
            .unwrap_or_else(|error| panic!("{original}: {error}"));
        assert_eq!(generated.cdef.trim_end(), c);
        assert_eq!(generated.rock.trim_end(), rock);

        let original_type = if let Some((_, param)) = signature.split_once(": ") {
            param.split([',', ')']).next().unwrap()
        } else {
            signature.split_once(" -> ").unwrap().1
        };
        let changed = source(&signature.replacen(original_type, mismatch, 1));
        match ffi_codegen::runtime::STONE_ENGINE.bindings([("", changed.as_str())]) {
            Ok(updated) => {
                assert_ne!(updated.cdef, generated.cdef);
                assert!(updated.cdef.contains(error), "{changed}: {}", updated.cdef);
            }
            Err(problem) => assert!(problem.contains(error), "{changed}: {problem}"),
        }
    }
}

#[test]
fn engine_projection_preserves_visibility_parameter_names_and_defaults() {
    let generated = ffi_codegen::runtime::STONE_ENGINE
        .bindings([(
            "",
            r#"
        /// rock-ffi stone private rename:_window=window default:wait=false
        #[no_mangle]
        pub extern "C" fn example(_window: *mut StoneWindow, wait: bool) -> i32 { 0 }
    "#,
        )])
        .unwrap();
    assert_eq!(
        generated.rock,
        "extern(ffi, \"stone\") fn example(window: StoneWindow, wait: bool = false) -> int\n"
    );
}

#[test]
fn generation_reads_top_level_exports_only() {
    let source = r##"
        const UNRELATED: &str = r#"
            /// rock-ffi stone-native
            #[no_mangle] extern "C" fn in_a_string() {}
        "#;
        mod nested {
            /// rock-ffi stone-native
            #[no_mangle] pub extern "C" fn in_a_module() {}
        }
        fn unrelated() {
            /// rock-ffi stone-native
            #[no_mangle] extern "C" fn in_a_body() {}
        }
        /// rock-ffi stone-native
        #[no_mangle] pub extern "C" fn example(sample: f32) -> f32 { sample }
    "##;
    let generated = STONE_NATIVE.bindings([("", source)]).unwrap();
    assert!(generated.cdef.ends_with("float example(float sample);\n"));
    assert_eq!(generated.rock.matches("extern(ffi,").count(), 1);
}

#[test]
fn a_void_pointer_can_have_a_typed_rock_view() {
    let source = r#"
        /// rock-ffi stone-rendering view:data=Vertex
        #[no_mangle]
        pub extern "C" fn example(data: *const c_void, stride: c_int) -> bool { true }
    "#;
    let module = ffi_codegen::rendering::STONE_RENDERING;
    let bindings = module.bindings([("probe", source)]).unwrap();
    assert_eq!(
        bindings.cdef,
        "uint8_t example(const void* data, int stride);\n"
    );
    assert_eq!(
        bindings.rock,
        "pub extern(ffi, \"stone\") fn example(data: Ptr<Vertex>, stride: int) -> bool\n"
    );
    assert!(bindings.rust.contains("*const ::std::ffi::c_void"));
    let changed = source.replace("*const c_void", "*const f32");
    let result = module.bindings([("probe", changed.as_str())]);
    assert!(matches!(result, Err(error) if error.contains("view requires a void pointer")));
}
