//! Entries for an embedding host that owns the frame loop.
//!
//! The host selects the mode with `stone_host_drives_frame_set` before the
//! game boots. `stone_run_game` then boots, publishes the frame entries here
//! and returns. Every frame the host calls `stone_pump_events`, then the
//! entries in `stone_game_entries()` (begin, update, draw), then
//! `stone_swap_window` when the draw result asks for a present. The host reads
//! the entries again each frame: a script reload publishes new ones.
//! Everything here is main-thread only.

use crate::{StoneEvent, StoneEventType};
use std::cell::{Cell, RefCell, UnsafeCell};
use std::ffi::{c_int, c_void};

pub const STONE_GAME_ENTRIES_ABI_VERSION: u32 = 1;

/// The host's input to `frame_begin`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct StoneFrameBeginInfo {
    pub struct_size: u32,
    pub _reserved: u32,
    /// The frame to open: 1 for the first, then the previous plan's `frame`
    /// plus one. A mismatch is rejected.
    pub frame: u64,
    /// `stone_get_time()` sampled by the host for this frame.
    pub time: f64,
}

/// Filled by `frame_begin` and completed by `frame_update`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct StoneFramePlan {
    pub struct_size: u32,
    /// `STONE_GAME_*`; 0 while running.
    pub termination: i32,
    /// The frame `frame_begin` opened; once the run has ended, the last frame.
    pub frame: u64,
    /// The dt the engine selected; 0 when the timeline does not tick.
    pub dt: f64,
    /// The scene's active size, valid after `frame_update` when `draws`.
    pub active_width: c_int,
    pub active_height: c_int,
    /// 0 once the run has ended; the host then calls `shutdown` and no
    /// further frame entry.
    pub running: u8,
    pub ticks: u8,
    /// Set by `frame_update`: the frame draws. When 0 the host skips its own
    /// world draw and still calls `frame_draw`.
    pub draws: u8,
    pub _reserved: [u8; 5],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct StoneFrameResult {
    pub struct_size: u32,
    pub termination: i32,
    pub running: u8,
    /// The guest drew this frame; the host swaps after drawing above it.
    pub present: u8,
    pub _reserved: [u8; 6],
}

/// Each entry returns 1, or 0 for a call out of order or invalid input; the
/// engine logs the reason.
pub type StoneGameFrameBeginFn =
    unsafe extern "C" fn(*const StoneFrameBeginInfo, *mut StoneFramePlan) -> i32;
pub type StoneGameFrameUpdateFn = unsafe extern "C" fn(*mut StoneFramePlan) -> i32;
pub type StoneGameFrameDrawFn = unsafe extern "C" fn(*mut StoneFrameResult) -> i32;
pub type StoneGameShutdownFn = unsafe extern "C" fn() -> i32;

/// The current frame entries; null until a host-driven game has booted and
/// again after its shutdown. `generation` changes on every publication.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct StoneGameEntries {
    pub struct_size: u32,
    pub abi_version: u32,
    pub generation: u64,
    pub frame_begin: Option<StoneGameFrameBeginFn>,
    pub frame_update: Option<StoneGameFrameUpdateFn>,
    pub frame_draw: Option<StoneGameFrameDrawFn>,
    pub shutdown: Option<StoneGameShutdownFn>,
}

const EMPTY_ENTRIES: StoneGameEntries = StoneGameEntries {
    struct_size: std::mem::size_of::<StoneGameEntries>() as u32,
    abi_version: STONE_GAME_ENTRIES_ABI_VERSION,
    generation: 0,
    frame_begin: None,
    frame_update: None,
    frame_draw: None,
    shutdown: None,
};

thread_local! {
    static HOST_DRIVES_FRAME: Cell<bool> = const { Cell::new(false) };
    // Boxed so the address the host holds stays put.
    static ENTRIES: Box<UnsafeCell<StoneGameEntries>> = Box::new(UnsafeCell::new(EMPTY_ENTRIES));
    static FRAME_EVENTS: RefCell<Vec<StoneEvent>> = const { RefCell::new(Vec::new()) };
    // The next buffered event `stone_poll_event` hands the guest.
    static GUEST_CURSOR: Cell<usize> = const { Cell::new(0) };
}

/// Drain the window's events into this frame's buffer, with the same side
/// effects as polling (key and pad state, imgui, window tracking). Call once
/// per frame before `frame_begin`. Returns the number of buffered events.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_pump_events() -> c_int {
    FRAME_EVENTS.with(|events| {
        let mut events = events.borrow_mut();
        events.clear();
        while let Some(Some(event)) = crate::with_window_mut(crate::poll_window_event) {
            if event.event_type != StoneEventType::None {
                events.push(event);
            }
        }
        GUEST_CURSOR.with(|cursor| cursor.set(0));
        events.len() as c_int
    })
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_frame_event_count() -> c_int {
    FRAME_EVENTS.with(|events| events.borrow().len() as c_int)
}

/// Copy buffered event `index` of this frame into `event`.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_frame_event(index: c_int, event: *mut StoneEvent) -> bool {
    if event.is_null() {
        return false;
    }
    let found = FRAME_EVENTS.with(|events| {
        usize::try_from(index)
            .ok()
            .and_then(|index| events.borrow().get(index).copied())
    });
    unsafe { *event = found.unwrap_or_default() };
    found.is_some()
}

/// The guest's side of `stone_poll_event` while the host drives the frame:
/// the next event of the pumped buffer.
pub(crate) fn guest_next_event() -> Option<StoneEvent> {
    GUEST_CURSOR.with(|cursor| {
        let index = cursor.get();
        let event = FRAME_EVENTS.with(|events| events.borrow().get(index).copied())?;
        cursor.set(index + 1);
        Some(event)
    })
}

/// Select whether the embedding host drives the frame. Call before the game
/// boots; the engine reads it once at boot.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_host_drives_frame_set(enabled: bool) {
    HOST_DRIVES_FRAME.with(|mode| mode.set(enabled));
}

/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_host_drives_frame() -> bool {
    HOST_DRIVES_FRAME.with(Cell::get)
}

/// The current frame entries. The pointer stays valid for the thread's
/// lifetime; read the entries through it every frame instead of keeping the
/// function pointers. Declared by hand in runtime_ffi.h.
#[no_mangle]
pub extern "C" fn stone_game_entries() -> *const StoneGameEntries {
    ENTRIES.with(|entries| entries.get() as *const StoneGameEntries)
}

/// Publish the frame entries (C function pointers with the
/// `StoneGameFrame*Fn` signatures); all null withdraws them. Returns the new
/// generation, or 0 with a logged error when only some are null.
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_game_entries_publish(
    frame_begin: *const c_void,
    frame_update: *const c_void,
    frame_draw: *const c_void,
    shutdown: *const c_void,
) -> u64 {
    let pointers = [frame_begin, frame_update, frame_draw, shutdown];
    let withdraw = pointers.iter().all(|p| p.is_null());
    if !withdraw && pointers.iter().any(|p| p.is_null()) {
        ::log::error!(target: "STONE", "stone_game_entries_publish: an entry is null");
        return 0;
    }
    ENTRIES.with(|entries| {
        // Main-thread only, and no reference into the cell is held across calls.
        let entries = unsafe { &mut *entries.get() };
        let generation = entries.generation + 1;
        *entries = if withdraw {
            StoneGameEntries {
                generation,
                ..EMPTY_ENTRIES
            }
        } else {
            // The caller passes functions with these signatures (runtime_ffi.h).
            unsafe {
                StoneGameEntries {
                    generation,
                    frame_begin: Some(std::mem::transmute::<*const c_void, StoneGameFrameBeginFn>(
                        frame_begin,
                    )),
                    frame_update: Some(
                        std::mem::transmute::<*const c_void, StoneGameFrameUpdateFn>(frame_update),
                    ),
                    frame_draw: Some(std::mem::transmute::<*const c_void, StoneGameFrameDrawFn>(
                        frame_draw,
                    )),
                    shutdown: Some(std::mem::transmute::<*const c_void, StoneGameShutdownFn>(
                        shutdown,
                    )),
                    ..EMPTY_ENTRIES
                }
            }
        };
        generation
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_layouts_are_stable() {
        assert_eq!(std::mem::size_of::<StoneFrameBeginInfo>(), 24);
        assert_eq!(std::mem::size_of::<StoneFramePlan>(), 40);
        assert_eq!(std::mem::offset_of!(StoneFramePlan, running), 32);
        assert_eq!(std::mem::size_of::<StoneFrameResult>(), 16);
        assert_eq!(std::mem::offset_of!(StoneGameEntries, frame_begin), 16);
        assert_eq!(std::mem::size_of::<StoneGameEntries>(), 48);
    }

    #[test]
    fn publication_replaces_or_withdraws_every_entry() {
        unsafe extern "C" fn begin(_: *const StoneFrameBeginInfo, _: *mut StoneFramePlan) -> i32 {
            1
        }
        unsafe extern "C" fn update(_: *mut StoneFramePlan) -> i32 {
            1
        }
        unsafe extern "C" fn draw(_: *mut StoneFrameResult) -> i32 {
            1
        }
        unsafe extern "C" fn shutdown() -> i32 {
            1
        }
        let entries = stone_game_entries();
        let fns = [
            begin as *const c_void,
            update as *const c_void,
            draw as *const c_void,
            shutdown as *const c_void,
        ];
        let first = stone_game_entries_publish(fns[0], fns[1], fns[2], fns[3]);
        assert_ne!(first, 0);
        let read = unsafe { *entries };
        assert_eq!(read.generation, first);
        assert!(read.frame_begin.is_some() && read.shutdown.is_some());
        let null = std::ptr::null();
        assert_eq!(stone_game_entries_publish(fns[0], null, fns[2], fns[3]), 0);
        assert_eq!(unsafe { *entries }.generation, first);
        let withdrawn = stone_game_entries_publish(null, null, null, null);
        assert_eq!(withdrawn, first + 1);
        let read = unsafe { *entries };
        assert!(read.frame_begin.is_none() && read.frame_draw.is_none());
        assert_eq!(stone_game_entries(), entries);
    }
}
