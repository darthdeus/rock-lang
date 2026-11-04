use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=SDL2_LIB_DIR");
    println!("cargo:rerun-if-changed=build.rs");

    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS");
    let profile = env::var("PROFILE").expect("PROFILE");

    // SDL2_LIB_DIR is an optional custom SDL2 development library directory;
    // otherwise the system linker locates SDL2.
    if let Some(value) = env::var_os("SDL2_LIB_DIR") {
        let lib_dir = PathBuf::from(value);
        assert!(
            lib_dir.is_absolute(),
            "SDL2_LIB_DIR must be an absolute path"
        );
        assert!(
            lib_dir.is_dir(),
            "SDL2_LIB_DIR must name an existing directory"
        );
        let lib_dir = lib_dir
            .to_str()
            .expect("SDL2_LIB_DIR must be representable as UTF-8 for Cargo");
        println!("cargo:rustc-link-search=native={lib_dir}");

        if profile == "debug" {
            match target_os.as_str() {
                "linux" | "macos" => println!("cargo:rustc-link-arg=-Wl,-rpath,{lib_dir}"),
                _ => {}
            }
        }
    }

    match target_os.as_str() {
        "linux" => println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN"),
        "macos" => println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path"),
        _ => {}
    }
}
