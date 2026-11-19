use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

use crate::handles::Class;

pub(crate) struct NativeClass {
    pub(crate) class: Class,
    pub(crate) name: &'static str,
    live: fn() -> Vec<u64>,
    destroy: fn(u64),
}

/// Owners first: a font owns its atlas image and a shader its uniform blocks. Reversing
/// this would delete a texture or buffer a live owner still uses.
pub(crate) const CLASSES: [NativeClass; Class::COUNT] = [
    NativeClass {
        class: Class::Font,
        name: "font",
        live: crate::text::live_font_ids,
        destroy: |h| {
            crate::text::stone_unload_font(h as u32);
        },
    },
    NativeClass {
        class: Class::Shader,
        name: "shader",
        live: || crate::SHADERS.with(|s| s.borrow().keys().copied().collect()),
        destroy: |h| crate::shader::stone_destroy_shader(h),
    },
    NativeClass {
        class: Class::Mesh,
        name: "mesh",
        live: crate::mesh::live_mesh_ids,
        destroy: |h| crate::mesh::stone_mesh_free(h),
    },
    NativeClass {
        class: Class::Canvas,
        name: "canvas",
        live: crate::canvas::live_canvas_ids,
        destroy: |h| crate::canvas::stone_destroy_canvas(h),
    },
    NativeClass {
        class: Class::ImageData,
        name: "image_data",
        live: crate::image_data::live_image_data_ids,
        destroy: |h| crate::image_data::stone_destroy_image_data(h),
    },
    NativeClass {
        class: Class::Image,
        name: "image",
        live: || crate::IMAGES.with(|i| i.borrow().keys().copied().collect()),
        destroy: destroy_image,
    },
    NativeClass {
        class: Class::Sound,
        name: "sound",
        live: crate::audio::live_sound_ids,
        destroy: |h| {
            crate::audio::stone_unload_sound(h);
        },
    },
    // After `sound`, which owns it: `stone_unload_sound` stops its voices.
    NativeClass {
        class: Class::Voice,
        name: "voice",
        live: crate::audio::live_voice_ids,
        destroy: |h| {
            crate::audio::stone_voice_stop(h);
        },
    },
    NativeClass {
        class: Class::Music,
        name: "music",
        live: crate::audio::live_music_ids,
        destroy: |h| {
            crate::audio::stone_unload_music(h);
        },
    },
    NativeClass {
        class: Class::PixelReadback,
        name: "pixel_readback",
        live: crate::pixel_readback::live_readback_ids,
        destroy: |h| crate::pixel_readback::stone_pixel_readback_destroy(h),
    },
    NativeClass {
        class: Class::Cursor,
        name: "cursor",
        live: crate::cursor::live_cursor_ids,
        destroy: |h| crate::cursor::stone_free_cursor(h),
    },
];

// Entry i must be variant i, or each class would answer with its neighbour's registry.
const _: () = {
    let mut i = 0;
    while i < Class::COUNT {
        assert!(
            CLASSES[i].class.index() == i,
            "CLASSES order must match Class"
        );
        i += 1;
    }
};

/// An `IMAGES` entry is not always the owner of its GL texture.
///
///  - a font's atlas image shares the font's texture; dropping the registry
///    entry and the font's cache slot is the whole teardown, and deleting the
///    texture here would blind a font the boot process still owns.
///  - the white image is minted lazily and owned only by that slot, so it is
///    destroyed for real and the slot cleared, which lets the next caller mint
///    it again exactly as a fresh process does.
fn destroy_image(handle: u64) {
    if crate::text::forget_atlas_image(handle) {
        return;
    }
    crate::WHITE_IMAGE.with(|wi| {
        let mut wi = wi.borrow_mut();
        if *wi == Some(handle) {
            *wi = None;
        }
    });
    crate::stone_unload_image(handle);
}

/// One minting class's id accounting: what it issued, what it still holds, and
/// what it recorded letting go.
struct ClassAccounting {
    name: &'static str,
    issued: u64,
    live: u64,
    retired: u64,
    /// Issued ids in neither the live set nor the retired count. A registry
    /// whose `live` fn under-reports lands here.
    unaccounted: i64,
    unissued: u64,
}

fn id_accounting() -> Vec<ClassAccounting> {
    CLASSES
        .iter()
        .filter(|c| c.class.mints())
        .map(|c| {
            let issued = crate::handles::issued(c.class);
            let retired = crate::handles::retired(c.class);
            let mut ids = (c.live)();
            ids.sort_unstable();
            ids.dedup();
            let live = ids.len() as u64;
            let unissued = ids.iter().filter(|&&id| id == 0 || id > issued).count() as u64;
            ClassAccounting {
                name: c.name,
                issued,
                live,
                retired,
                unaccounted: issued as i64 - live as i64 - retired as i64,
                unissued,
            }
        })
        .collect()
}

#[no_mangle]
pub extern "C" fn stone_native_id_accounting() -> *const c_char {
    let rows = id_accounting();
    let sum = |f: fn(&ClassAccounting) -> u64| rows.iter().map(f).sum::<u64>();
    let mut out = format!(
        "classes={} issued={} live={} retired={} unaccounted={} unissued={}",
        rows.len(),
        sum(|r| r.issued),
        sum(|r| r.live),
        sum(|r| r.retired),
        rows.iter().map(|r| r.unaccounted).sum::<i64>(),
        sum(|r| r.unissued),
    );
    for r in &rows {
        out.push_str(&format!(
            "; {}:issued={},live={},retired={},unaccounted={},unissued={}",
            r.name, r.issued, r.live, r.retired, r.unaccounted, r.unissued,
        ));
    }
    reply(out)
}

fn class_by_name(name: &str) -> Option<&'static NativeClass> {
    CLASSES.iter().find(|c| c.name == name)
}

fn class_arg(class: *const c_char) -> Option<&'static NativeClass> {
    if class.is_null() {
        return None;
    }
    class_by_name(unsafe { CStr::from_ptr(class) }.to_str().ok()?)
}

thread_local! {
    static REPLY: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn reply(s: String) -> *const c_char {
    REPLY.with(|b| {
        let c = match CString::new(s) {
            Ok(c) => c,
            Err(_) => return std::ptr::null(),
        };
        let ptr = c.as_ptr();
        *b.borrow_mut() = Some(c);
        ptr
    })
}

#[no_mangle]
pub extern "C" fn stone_native_live_handles(class: *const c_char) -> *const c_char {
    let Some(class) = class_arg(class) else {
        return std::ptr::null();
    };
    let mut ids = (class.live)();
    ids.sort_unstable();
    reply(
        ids.iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(","),
    )
}

#[no_mangle]
pub extern "C" fn stone_native_handle_state(class: *const c_char, handle: u64) -> c_int {
    match class_arg(class) {
        Some(class) => c_int::from((class.live)().contains(&handle)),
        None => -1,
    }
}

#[no_mangle]
pub extern "C" fn stone_native_destroy(class: *const c_char, handle: u64) -> bool {
    let Some(class) = class_arg(class) else {
        return false;
    };
    (class.destroy)(handle);
    !(class.live)().contains(&handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ledger_class_mints_through_the_seam_except_the_one_that_cannot() {
        let non_minting: Vec<&str> = CLASSES
            .iter()
            .filter(|c| !c.class.mints())
            .map(|c| c.name)
            .collect();
        assert_eq!(
            non_minting,
            vec!["font"],
            "a class outside the mint seam is invisible to id accounting; \
             font is the only one whose id is a vec index rather than a number"
        );
        assert_eq!(
            id_accounting().len(),
            CLASSES.len() - 1,
            "and the census examines exactly the classes that do mint"
        );
    }
}
