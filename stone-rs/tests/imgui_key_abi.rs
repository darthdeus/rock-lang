#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::mem::{size_of, transmute_copy};

unsafe fn required_symbol<T: Copy>(handle: *mut c_void, name: &CStr) -> T {
    let symbol = libc::dlsym(handle, name.as_ptr());
    assert!(!symbol.is_null(), "missing exported symbol {:?}", name);
    assert_eq!(size_of::<T>(), size_of::<*mut c_void>());
    transmute_copy(&symbol)
}

#[test]
fn imgui_tool_key_symbols_export_the_current_c_abi() {
    let artifact_name = if cfg!(target_os = "macos") {
        "libstone_rs.dylib"
    } else {
        "libstone_rs.so"
    };
    let artifact = std::env::current_exe()
        .expect("locate test executable")
        .parent()
        .expect("test executable directory")
        .join(artifact_name);
    let artifact = CString::new(artifact.to_str().expect("UTF-8 artifact path"))
        .expect("artifact path contains no NUL");
    let handle = unsafe { libc::dlopen(artifact.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    if handle.is_null() {
        let error = unsafe { libc::dlerror() };
        let detail = if error.is_null() {
            "unknown dynamic loader error".to_owned()
        } else {
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        };
        panic!("failed to load Stone cdylib: {detail}");
    }

    unsafe {
        let key_id: unsafe extern "C" fn(*const c_char) -> c_int =
            required_symbol(handle, c"stone_imgui_key_id");
        let is_key_down: unsafe extern "C" fn(c_int) -> bool =
            required_symbol(handle, c"stone_imgui_is_key_down");
        let ctrl_down: unsafe extern "C" fn() -> bool =
            required_symbol(handle, c"stone_imgui_ctrl_down");
        let inject_key: unsafe extern "C" fn(c_int) =
            required_symbol(handle, c"stone_imgui_inject_key");
        let release_key: unsafe extern "C" fn(c_int) =
            required_symbol(handle, c"stone_imgui_release_key");

        let left_ctrl = CString::new("lctrl").unwrap();
        let letter_a = CString::new("a").unwrap();
        let return_key = CString::new("return").unwrap();
        let function_key = CString::new("F5").unwrap();
        let backslash = CString::new("\\").unwrap();
        let unknown = CString::new("not-a-key").unwrap();
        {
            assert_eq!(key_id(left_ctrl.as_ptr()), 527);
            assert_eq!(key_id(letter_a.as_ptr()), 546);
            assert_eq!(key_id(return_key.as_ptr()), 525);
            assert_eq!(key_id(function_key.as_ptr()), 576);
            assert_eq!(key_id(c"[".as_ptr()), 603);
            assert_eq!(key_id(backslash.as_ptr()), 604);
            assert_eq!(key_id(unknown.as_ptr()), -1);
            assert_eq!(key_id(std::ptr::null()), -1);
        }

        assert!(!is_key_down(546));
        assert!(!ctrl_down());
        inject_key(546);
        release_key(546);

        libc::dlclose(handle);
    }
}
