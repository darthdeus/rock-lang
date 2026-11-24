//! Build script for stone-rs
//!
//! Configures SDL2 to link against our prebuilt libraries.
//! Uses CARGO_CFG_TARGET_* for correct cross-compilation support.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

mod nav_ffi_codegen;

fn navigation_ffi() {
    let rock = Path::new("../packages/stone-native");
    let rust = "src/nav/ffi.rs";
    let header = "../packages/stone-native/stone_native.h";
    for path in [
        rust,
        "nav_ffi_codegen.rs",
        "../packages/stone-native",
        "../packages/stone-core",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let read = |path| fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let sources = nav_ffi_codegen::load_rock(rock)
        .unwrap_or_else(|e| panic!("load Stone native package: {e}"));
    let generated = nav_ffi_codegen::generate(&sources, &read(rust))
        .unwrap_or_else(|e| panic!("navigation FFI: {e}"));

    let existing = read(header);
    let marker = "// Generated navigation declarations. See stone-rs/build.rs.\n";
    let (legacy, declarations) = existing
        .split_once(marker)
        .expect("navigation declaration marker in stone_native.h");
    if declarations != generated.cdef {
        let updated = out.join("stone_native.h");
        fs::write(&updated, format!("{legacy}{marker}{}", generated.cdef)).unwrap();
        panic!(
            "stale navigation FFI declarations in {header}; copy {} over the header",
            updated.display()
        );
    }
    fs::write(out.join("stone_nav_ffi.rs"), generated.rust).unwrap();
}

fn main() {
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .stack_size(rock_core::nesting::COMPILER_STACK_BYTES)
        .build()
        .expect("compiler worker")
        .install(navigation_ffi);

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
