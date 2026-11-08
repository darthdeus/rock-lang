use std::fs;
use std::path::Path;

// Counts `declaration` only where it is not a prefix of a longer identifier,
// so a new `fn live_texture_bytes_*` does not count as `fn live_texture_bytes`.
fn declaration_count(source: &str, declaration: &str) -> usize {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    source
        .match_indices(declaration)
        .filter(|(at, _)| {
            let before = source[..*at].chars().next_back();
            let after = source[at + declaration.len()..].chars().next();
            !before.is_some_and(is_ident)
                && !(declaration.ends_with(is_ident) && after.is_some_and(is_ident))
        })
        .count()
}

#[test]
fn image_texture_owns_the_native_image_surface() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let lib = fs::read_to_string(root.join("src/lib.rs")).expect("read lib.rs");
    let image_texture = fs::read_to_string(root.join("src/image_texture.rs")).unwrap_or_default();

    let declarations = [
        "pub(crate) struct LoadedImage",
        "pub(crate) static IMAGES",
        "pub(crate) static WHITE_IMAGE",
        "const IMAGE_HEADER_PREFIX",
        "pub(crate) fn decode_rgba_image",
        "fn resolve_image_bytes",
        "pub extern \"C\" fn stone_load_image",
        "pub extern \"C\" fn stone_flush_image_uploads",
        "pub extern \"C\" fn stone_set_frame_image_upload_budget",
        "pub extern \"C\" fn stone_image_uploads_pending",
        "pub extern \"C\" fn stone_service_image_uploads",
        "pub struct ImageLoadStats",
        "pub extern \"C\" fn stone_reset_image_profile_stats()",
        "pub extern \"C\" fn stone_reset_image_profile_stats_to_frame()",
        "pub extern \"C\" fn stone_image_frame_activity",
        "pub extern \"C\" fn stone_get_image_frame_stats",
        "pub extern \"C\" fn stone_get_image_profile_stats",
        "fn upload_decoded",
        "pub extern \"C\" fn stone_get_image_size",
        "pub extern \"C\" fn stone_image_set_wrap",
        "fn image_write_target(",
        "fn image_write_target_stored(",
        "static CAPPED_STORAGE",
        "fn stored_size",
        "fn record_storage",
        "const STUB_STORAGE",
        "fn load_storage_dims",
        "fn register_loaded_image",
        "fn define_image_storage",
        "pub extern \"C\" fn stone_image_read_texel",
        "pub extern \"C\" fn stone_image_wrap_state",
        "pub extern \"C\" fn stone_image_gl_wrap_state",
        "pub extern \"C\" fn stone_bind_image_texture_to_unit",
        "pub extern \"C\" fn stone_get_image_texture",
        "static STUB_PATHS",
        "fn stub_path",
        "pub extern \"C\" fn stone_unload_image",
        "pub extern \"C\" fn stone_reload_image",
        "pub extern \"C\" fn stone_get_white_image",
        "fn live_texture_bytes(",
    ];

    let owned = declarations
        .iter()
        .filter(|decl| declaration_count(&image_texture, decl) == 1)
        .count();
    assert!(
        owned > 0,
        "image-texture source boundary found no owned symbols"
    );
    assert_eq!(
        owned,
        declarations.len(),
        "image-texture symbol population changed"
    );
    assert_eq!(lib.matches("mod image_texture;").count(), 1);

    for declaration in declarations {
        assert_eq!(
            declaration_count(&image_texture, declaration),
            1,
            "expected one image-texture owner declaration: {declaration}"
        );
        assert_eq!(
            declaration_count(&lib, declaration),
            0,
            "lib.rs still owns image-texture declaration: {declaration}"
        );
    }
}
