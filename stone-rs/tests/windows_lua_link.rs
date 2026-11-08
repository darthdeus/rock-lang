#[test]
fn windows_cdylib_links_the_shipped_lua_runtime() {
    let build =
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"))
            .unwrap();

    assert!(
        build
            .lines()
            .any(|line| line.trim() == r#"println!("cargo:rustc-link-lib=dylib=lua51");"#),
        "stone_rs.dll must resolve its Lua allocator imports through lua51.lib"
    );
}
