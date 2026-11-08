//! Exact live-handle sets for the native resource classes, a per-handle
//! liveness query, a teardown that reports whether the handle is gone, and id
//! accounting that checks each class's live set against the allocator.
//!
//! It publishes the registries that already exist rather than adding one. Every
//! class here is a map or slot table the engine keeps for its own reasons.
//!
//! Handles are compared as exact SETS, never as counts, and that is sound
//! because there is ONE id allocator (`handles::mint`), monotonic per class --
//! an id is never handed out twice in one process, so a destroyed-and-recreated
//! resource is a DIFFERENT key rather than the same one. Fonts are the one
//! ledger class outside it, and cannot join: a font id is its 1-based slot in
//! the `FONTS` vec, so the vec IS the allocator. `stone_proc` is absent from
//! the ledger for the same reason -- its handles are RECYCLED slots.
//!
//! Class ORDER is ownership order. Two entries in `IMAGES` are projections of a
//! resource owned elsewhere -- a font's atlas and the lazily-minted white
//! texture -- so their owner is torn down first and the image teardown knows
//! about the aliasing rather than deleting a texture still in use. A sound
//! precedes the voices playing it for the same reason -- dropping the sound
//! stops them.
//!
//! The class list must be a COMPLETE inventory of what this crate owns, and a
//! live-set reader cannot check that for itself: it is empty for a registry it
//! never asks about. Two mechanisms make omission fail closed instead.
//!
//! `handles::Class` sizes `CLASSES`, so a variant with no entry and an entry
//! with no variant are both compile errors, and a const assertion pins entry
//! `i` to variant `i` so the lengths cannot agree while the rows are shifted.
//! That covers the list. It does not cover the REGISTRY behind a row, so
//! `id_accounting` crosses each class's published live set with what
//! `handles::mint` independently recorded issuing it. An id the seam issued
//! that the class cannot show is `unaccounted`; a live handle the seam never
//! minted is `unissued`. Driven by `b3_id_accounting` in testbed.
//!
//! Both mechanisms are complete only over classes that MINT, so everything
//! else is named in `NON_MINTING` with what covers it instead. Uniform blocks
//! are the deliberate omission from the CLASS list: `BLOCKS` is keyed by SHADER
//! handle, so `stone_destroy_shader` owns a program's whole resource set.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

use crate::handles::Class;

pub(crate) struct NativeClass {
    /// The `Class` variant this entry IS. Its discriminant is this entry's
    /// index, asserted below, so the two lists cannot drift into a silent
    /// off-by-one where every class answers with its neighbour's registry.
    pub(crate) class: Class,
    pub(crate) name: &'static str,
    live: fn() -> Vec<u64>,
    destroy: fn(u64),
}

/// Owners first: tearing a font down removes its atlas image, tearing a shader
/// down removes its uniform blocks. Reversing this order would delete a texture
/// or buffer that a still-live owner points at.
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
    // AFTER `sound`, which owns it: `stone_unload_sound` stops its voices.
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

/// The array is sized by `Class::COUNT`, so a variant with no entry and an
/// entry with no variant are both compile errors. This pins the remaining
/// degree of freedom: entry `i` must BE variant `i`, or every class would
/// answer with its neighbour's registry while the lengths still agreed.
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
    /// Live ids outside `1..=issued`, i.e. handles this class is publishing
    /// that the seam never handed it. A registry minting from its own counter
    /// lands here.
    unissued: u64,
}

/// Ask every minting class whether its published live set accounts for the ids
/// it was issued.
///
/// This is the question the set diff cannot ask. The diff compares one class's
/// live set at two TIMES, so a `live` fn that reports the wrong set reports it
/// wrongly at both and the comparison is happy; and it never asks a registry
/// that joined the crate without joining `CLASSES` anything at all. Accounting
/// crosses the class's own answer with the allocator's independent record, so
/// the two have to agree about a population neither one owns alone.
///
/// Both directions are checked because they fail differently. `unaccounted` is
/// an id the seam issued that the class can no longer show -- under-reported
/// live set, or a destroy that removed the entry without retiring it.
/// `unissued` is a live handle the seam never minted -- a class that went back
/// to a private counter, which leaves accounting looking green on the first
/// number alone.
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

/// The id accounting for every minting class, as one line: a summary with the
/// denominators the caller must assert, then one row per class.
///
/// Every class is reported, not only the offenders -- a report that lists only
/// violations is indistinguishable from a report that examined nothing.
///
/// One KNOWN non-ledger source of `unaccounted`, so a caller does not
/// mis-diagnose it: under `STONE_AUDIO_STUB=1` (the default for sims)
/// `stone_load_sound` mints a sound id and returns it to Rock while
/// `AUDIO_STATE` is None, so nothing is inserted and the class publishes an
/// empty live set. Measured on bitgun's boot: 23 unaccounted stubbed, 0 with
/// `STONE_AUDIO_STUB=0`. Pinned by `b3_id_accounting_stub_sound_sentinel`.
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

/// Registries this crate owns whose state id accounting cannot see, each with
/// what covers it instead.
///
/// Id accounting is complete only over classes that mint, so a registry keyed
/// by anything else -- a path, a driver-assigned name, a raw pointer -- is
/// invisible to it by construction. That is a real hole and this is the list of
/// it, pinned by a test so a new non-minting registry is a decision someone
/// made rather than one nobody noticed. Adding a row here is the honest move;
/// the dishonest one is a registry that appears in neither list.
/// A declaration read by people and pinned by a test; it has no runtime caller
/// by design, and adding one would make it a mechanism rather than a list.
#[allow(dead_code)]
pub(crate) const NON_MINTING: &[(&str, &str)] = &[
    (
        "uniform_block",
        "covered: BLOCKS is keyed by SHADER handle, so a block rides the \
         shader class -- stone_destroy_shader owns the program's whole \
         resource set. BLOCK_PROGRAMS is keyed by driver-RECYCLED GL program ids, \
         outside the monotonic-id argument, and is sound only because removal \
         in destroy_blocks_for_shader is unconditional.",
    ),
    (
        "asset_watcher",
        "outside by design: REGISTERED/CHANGED/watched_dirs are path-keyed \
         process state with no handle to account for.",
    ),
    (
        "log_dedup",
        "outside by design: rock-log's log_once!/log_throttled! stores are \
         call-site keyed process state with no handle to account for.",
    ),
    (
        "stone_proc",
        "outside by design: handles are RECYCLED slots, so they are not ids in \
         the monotonic sense the ledger's set comparison needs.",
    ),
];

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

/// The shared reply buffer, for a sibling module publishing a string reading.
pub(crate) fn watcher_reply(s: String) -> *const c_char {
    reply(s)
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

/// How many resource classes the ledger covers. Callers enumerate the engine's
/// list rather than keeping their own -- a second list is a second thing to
/// forget a class in.
#[no_mangle]
pub extern "C" fn stone_native_class_count() -> c_int {
    CLASSES.len() as c_int
}

/// Name of class `index`, or null when out of range.
#[no_mangle]
pub extern "C" fn stone_native_class_name(index: c_int) -> *const c_char {
    match CLASSES.get(index as usize) {
        Some(c) => reply(c.name.to_string()),
        None => std::ptr::null(),
    }
}

/// The live handle set of one class, ascending, comma separated; the empty
/// string for a class with nothing live. Null for an unknown class name, so a
/// caller asking about a class that does not exist gets a refusal rather than
/// an empty set that reads as "nothing leaked".
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

/// Whether one handle of one class is live: 1 live, 0 not live, -1 the CLASS is
/// not one this engine has.
///
/// Three states and not two. A boolean forces an unknown class to answer "not
/// live", which reads exactly like a dangling handle -- so a caller whose class
/// names drifted from this list would silently report every handle of that type
/// as missing, and be believed.
#[no_mangle]
pub extern "C" fn stone_native_handle_state(class: *const c_char, handle: u64) -> c_int {
    match class_arg(class) {
        Some(class) => c_int::from((class.live)().contains(&handle)),
        None => -1,
    }
}

/// Destroy one handle of one class and report whether it is gone afterwards.
/// The answer is read back from the live set rather than from the destroy call,
/// so a teardown that silently no-ops cannot report success.
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

    /// Which ledger classes draw their ids from the mint seam, pinned by name.
    ///
    /// `Class::COUNT` sizes `CLASSES`, so the two lists cannot disagree about
    /// how many classes exist -- but nothing structural says a class MINTS.
    /// A new class that quietly answers false here would be admitted to the
    /// ledger while id accounting silently skipped it, which is the one hole
    /// the enum leaves open. Font is the only legitimate false, and it is
    /// forced: its id is a `FONTS` slot index the atlas path dereferences.
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

    /// The registries this crate owns that id accounting cannot see, pinned so
    /// a new non-minting registry is a decision rather than an omission.
    ///
    /// Accounting is complete only over classes that mint, and everything else
    /// has to be named HERE with what covers it instead. A registry in neither list is the failure mode -- and the only
    /// thing that can catch it is a reader noticing, which is why the coverage
    /// status is spelled out per row rather than left to the name.
    #[test]
    fn the_registries_id_accounting_cannot_see_are_declared_with_their_coverage() {
        let names: Vec<&str> = NON_MINTING.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                // Rides the `shader` class; BLOCKS is keyed by shader handle.
                "uniform_block",
                // Path-keyed inotify watches.
                "asset_watcher",
                // Call-site-keyed dedup stores.
                "log_dedup",
                // Recycled slots by design.
                "stone_proc",
            ],
            "every registry outside the mint seam is named with its coverage"
        );
        for (name, coverage) in NON_MINTING {
            assert!(
                coverage.starts_with("covered:")
                    || coverage.starts_with("UNCOVERED:")
                    || coverage.starts_with("outside by design:"),
                "{name}: coverage must declare covered / UNCOVERED / outside by design"
            );
        }
        assert_eq!(
            NON_MINTING
                .iter()
                .filter(|(_, c)| c.starts_with("UNCOVERED:"))
                .count(),
            0,
            "every registry outside the mint seam is covered or outside by design"
        );
    }
}
