use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn source(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(root().join(path)).unwrap()
}

fn visit_rock_sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            visit_rock_sources(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rock") {
            found.push(path);
        }
    }
}

#[test]
fn directory_listing_has_one_typed_fallible_cross_backend_api() {
    let native = source("stone/engine/lib.rock");
    assert!(
        native.contains("fn stone_list_directory(path: string) -> [string]?"),
        "native directory listing must expose one typed fallible contract"
    );
    let web = source("stone/engine/web/js_ffi.rock");
    assert!(
        web.contains("fn stone_list_directory(path: string) -> [string]?"),
        "web must implement the same typed contract"
    );

    for path in [
        "stone/engine/compat/assets.rock",
        "stone/engine/compat/shaders.rock",
        "stone/engine/debug/font_browser.rock",
    ] {
        let text = source(path);
        assert!(
            !text.contains("stone.stone_list_directory("),
            "{path} bypasses the shared directory-listing API"
        );
    }
    assert!(!source("stone/engine/debug/font_browser.rock").contains("fb_list_dir"));
    assert!(!source("stone/engine/timeline.rock").contains("__strvec"));
    assert!(!source("stone/engine/compat/assets.rock").contains("__asset_list_dir_impl"));
    assert!(!web.contains("fb_list_dir"));
    assert!(!web.contains("__asset_list_dir_impl"));
    assert!(web.contains("__sw_log_once(\"stone_list_directory failed for \" + path)"));
    assert!(!web.contains("__stone_list_directory_warned"));

    assert!(
        source("libs/fm-core/fs_seam.rock")
            .contains("list_dir:     fn(string) -> [string]?"),
        "FmFs must use the canonical typed listing shape"
    );

    let mut bypasses = Vec::new();
    for top in ["bitgun", "games", "libs", "stone"] {
        let mut paths = Vec::new();
        visit_rock_sources(&root().join(top), &mut paths);
        for path in paths {
            if path.ends_with("stone/engine/lib.rock") {
                continue;
            }
            if std::fs::read_to_string(&path).unwrap().contains("stone.stone_list_directory(") {
                bypasses.push(path.strip_prefix(root()).unwrap().display().to_string());
            }
        }
    }
    assert!(bypasses.is_empty(), "raw directory-listing FFI bypasses: {bypasses:?}");
}

#[test]
fn native_asset_overlays_consume_the_defaults_injected_by_the_cli() {
    assert!(
        source("stone/engine/compat/assets.rock")
            .contains("ipairs(_G.__ROCK_ASSET_OVERLAY_DEFAULTS or {})"),
        "native runtime must consume the [assets].overlays defaults emitted in the CLI preamble"
    );
}

#[test]
fn directory_cleanup_and_registry_lookup_keep_their_explicit_contracts() {
    let shaders = source("stone/engine/compat/shaders.rock");
    assert!(shaders.contains("fn io_clear_directory_recursive(path: string)"));
    assert!(!shaders.contains("fn io_clear_directory(path: string)"));
    assert!(shaders.contains("io_clear_directory:create:{path}"));
    assert!(shaders.contains("io_clear_directory:entry:{child}"));

    let registry = source("stone-rs/src/asset_registry.rs");
    assert!(registry.contains("registry.directories.contains_key(&key)"));
    assert!(!registry.contains("map.keys().any(|key| key.starts_with(&prefix))"));
}
