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

fn stone_native_ffi() {
    use ffi_codegen::{GENERATED, STONE_NATIVE, STONE_NATIVE_SOURCES};

    let rock = Path::new("../packages/stone-native");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    for path in [
        "ffi_codegen.rs",
        "../packages/stone-native/stone_native.h",
        "../packages/stone-native/bindings.rock",
        "../packages/stone-native/wire_layouts.rock",
        "../packages/stone-native/tests/ffi_layouts.rock",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let sources: Vec<_> = STONE_NATIVE_SOURCES
        .iter()
        .map(|(file, module)| {
            let path = Path::new("src").join(file);
            println!("cargo:rerun-if-changed={}", path.display());
            (*module, read(&path))
        })
        .collect();
    let generated = STONE_NATIVE
        .bindings(
            sources
                .iter()
                .map(|(module, text)| (*module, text.as_str())),
        )
        .unwrap_or_else(|e| panic!("Stone native FFI: {e}"));
    let header = rock.join("stone_native.h");
    let existing = read(&header);
    let (inline_lua, _) = existing
        .split_once(GENERATED)
        .expect("FFI declaration marker");
    let mut stale = Vec::new();
    check_generated(
        &header,
        format!("{inline_lua}{GENERATED}{}", generated.cdef),
        &out,
        &mut stale,
    );
    check_generated(
        &rock.join("bindings.rock"),
        format!("{GENERATED}{}", generated.rock),
        &out,
        &mut stale,
    );
    assert!(
        stale.is_empty(),
        "stale Stone native ABI:\n{}",
        stale.join("\n")
    );

    let layout_source = rock.join("wire_layouts.rock");
    let (layouts, tests) = STONE_NATIVE
        .layouts("wire_layouts.rock", &read(&layout_source))
        .unwrap_or_else(|e| panic!("Stone native layout: {e}"));
    check_generated(
        &rock.join("tests/ffi_layouts.rock"),
        tests,
        &out,
        &mut stale,
    );
    assert!(
        stale.is_empty(),
        "stale Stone native layouts:\n{}",
        stale.join("\n")
    );

    let file = out.join("stone_native_ffi.rs");
    let rust = format!("{layouts}{}", generated.rust);
    if !fs::read_to_string(&file)
        .as_deref()
        .is_ok_and(|stored| stored == rust)
    {
        fs::write(file, rust).unwrap();
    }
}

fn main() {
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .stack_size(rock_core::nesting::COMPILER_STACK_BYTES)
        .build()
        .expect("compiler worker")
        .install(stone_native_ffi);

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
