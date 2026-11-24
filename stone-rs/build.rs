//! Build script for stone-rs
//!
//! Configures SDL2 to link against our prebuilt libraries.
//! Uses CARGO_CFG_TARGET_* for correct cross-compilation support.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

mod ffi_codegen;

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn check_generated(path: &Path, generated: String, out: &Path, stale: &mut Vec<String>) {
    if fs::read_to_string(path)
        .as_deref()
        .is_ok_and(|stored| stored == generated)
    {
        return;
    }
    let updated = out.join(path.file_name().unwrap());
    fs::write(&updated, generated).unwrap();
    stale.push(format!("copy {} to {}", updated.display(), path.display()));
}

type CLayoutGenerator = fn(&str) -> Result<String, String>;

fn package_ffi(
    module: &ffi_codegen::Module,
    root: &str,
    header: &str,
    bindings: &str,
    sources: &[(&str, &str)],
    layout_files: Option<(&str, &str)>,
    c_owned_layout: Option<CLayoutGenerator>,
) {
    use ffi_codegen::GENERATED;

    let rock = Path::new(root);
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR")).join(module.package);
    fs::create_dir_all(&out).unwrap();
    for path in [header, bindings] {
        println!("cargo:rerun-if-changed={}", rock.join(path).display());
    }
    let sources: Vec<_> = sources
        .iter()
        .map(|(file, module)| {
            let path = Path::new("src").join(file);
            println!("cargo:rerun-if-changed={}", path.display());
            (*module, read(&path))
        })
        .collect();
    let generated = module
        .bindings(
            sources
                .iter()
                .map(|(module, text)| (*module, text.as_str())),
        )
        .unwrap_or_else(|e| panic!("{} FFI: {e}", module.package));
    let header = rock.join(header);
    let existing = read(&header);
    let (authored, _) = existing
        .split_once(GENERATED)
        .expect("FFI declaration marker");
    let rock_bindings = rock.join(bindings);
    let existing_rock = read(&rock_bindings);
    let (authored_rock, _) = existing_rock
        .split_once(GENERATED)
        .expect("FFI declaration marker");
    let mut stale = Vec::new();
    check_generated(
        &header,
        format!("{authored}{GENERATED}{}", generated.cdef),
        &out,
        &mut stale,
    );
    check_generated(
        &rock_bindings,
        format!("{authored_rock}{GENERATED}{}", generated.rock),
        &out,
        &mut stale,
    );
    assert!(
        stale.is_empty(),
        "stale {} ABI:\n{}",
        module.package,
        stale.join("\n")
    );

    let mut layouts = String::new();
    if let Some((layout_source, layout_test)) = layout_files {
        let source = rock.join(layout_source);
        let layout_test = rock.join(layout_test);
        for path in [&source, &layout_test] {
            println!("cargo:rerun-if-changed={}", path.display());
        }
        let (rust, tests) = module
            .layouts(layout_source, &read(&source))
            .unwrap_or_else(|e| panic!("{} layout: {e}", module.package));
        layouts = rust;
        check_generated(
            &layout_test,
            format!("{GENERATED}{tests}"),
            &out,
            &mut stale,
        );
        assert!(
            stale.is_empty(),
            "stale {} layouts:\n{}",
            module.package,
            stale.join("\n")
        );
    }

    let file = out.join("ffi.rs");
    let c_layout = c_owned_layout
        .map(|generate| {
            generate(authored).unwrap_or_else(|e| panic!("{} layout: {e}", module.package))
        })
        .unwrap_or_default();
    let rust = format!("{layouts}{c_layout}{}", generated.rust);
    if !fs::read_to_string(&file)
        .as_deref()
        .is_ok_and(|stored| stored == rust)
    {
        fs::write(file, rust).unwrap();
    }
}

fn main() {
    use ffi_codegen::{
        rendering::{STONE_RENDERING, STONE_RENDERING_LAYOUT_FILES, STONE_RENDERING_SOURCES},
        runtime::{STONE_ENGINE, STONE_ENGINE_LAYOUT_FILES, STONE_ENGINE_SOURCES},
        STONE_NATIVE, STONE_NATIVE_SOURCES,
    };

    println!("cargo:rerun-if-changed=ffi_codegen.rs");
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .stack_size(rock_core::nesting::COMPILER_STACK_BYTES)
        .build()
        .expect("compiler worker")
        .install(|| {
            package_ffi(
                &STONE_NATIVE,
                "../packages/stone-native",
                "stone_native.h",
                "bindings.rock",
                STONE_NATIVE_SOURCES,
                Some(("wire_layouts.rock", "tests/ffi_layouts.rock")),
                None,
            );
            package_ffi(
                &STONE_ENGINE,
                "../packages/stone",
                "engine/runtime_ffi.h",
                "engine/runtime_ffi.rock",
                STONE_ENGINE_SOURCES,
                STONE_ENGINE_LAYOUT_FILES,
                Some(ffi_codegen::stone_event_layout),
            );
            package_ffi(
                &STONE_RENDERING,
                "../packages/stone",
                "engine/rendering_ffi.h",
                "engine/rendering_ffi.rock",
                STONE_RENDERING_SOURCES,
                STONE_RENDERING_LAYOUT_FILES,
                None,
            );
        });

    // Target (not host) platform, for cross-compilation: CARGO_CFG_TARGET_OS is
    // set by Cargo to the platform being compiled FOR.
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();

    // The per-target table is in `gamelibs`, shared with the other crates that
    // link SDL2.
    let prebuilt_dir = gamelibs::prebuilt_dir();

    // Tell the linker where to find SDL2 and cimgui
    println!(
        "cargo:rustc-link-search=native={}",
        prebuilt_dir.join("lib").display()
    );
    println!("cargo:rustc-link-lib=dylib=cimgui_complete");
    if target_os == "windows" {
        // PE DLLs cannot resolve the allocator probe's lua_* imports from the host process.
        println!("cargo:rustc-link-lib=dylib=lua51");
    }

    // Set rpath for runtime library loading
    match target_os.as_str() {
        "linux" => {
            println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
            println!(
                "cargo:rustc-link-arg=-Wl,-rpath,{}",
                prebuilt_dir.join("lib").display()
            );
        }
        "macos" => {
            // libstone is loaded by LuaJIT, which owns the lua_* symbols used
            // by the allocator probe; resolve them from that process at load.
            // This must cover every own-package link, not just the cdylib:
            // the test binaries carry the same undefined lua_* references
            // (the allocator probe reaches lua_setallocf) and macOS ld rejects those in executables, so without
            // this `cargo test -p stone-rs --lib` fails with an undefined
            // _lua_setallocf unless the caller passes RUSTFLAGS. There is no
            // test-scoped link-arg directive, but plain rustc-link-arg does
            // not propagate downstream, so shipped bins are unaffected.
            println!("cargo:rustc-link-arg=-Wl,-undefined,dynamic_lookup");
            println!(
                "cargo:rustc-link-arg=-Wl,-rpath,{}",
                prebuilt_dir.join("lib").display()
            );
            // Release layout: the native libraries are beside libstone.
            println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");
        }
        _ => {}
    }

    println!("cargo:rerun-if-changed=build.rs");
}
