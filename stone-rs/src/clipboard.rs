#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
#[no_mangle]
pub extern "C" fn stone_clipboard_image_data() -> u64 {
    let image = match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_image()) {
        Ok(image) => image,
        Err(_) => return 0,
    };
    crate::image_data::store_owned_rgba(image.width, image.height, image.bytes.into_owned())
        .unwrap_or(0)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[no_mangle]
pub extern "C" fn stone_clipboard_image_data() -> u64 {
    0
}
