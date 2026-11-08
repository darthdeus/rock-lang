//! Optional host capability. Never infer native counters from Lua callbacks.
use crate::jit_health_abi::{self as abi, Snapshot};
use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::OnceLock;

struct Api {
    read: abi::Read,
    category: Option<abi::Category>,
    configure: Option<abi::Configure>,
    coalescing: Option<abi::Coalescing>,
}

unsafe fn symbol(name: &CStr) -> *mut c_void {
    #[cfg(unix)]
    {
        libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr())
    }
    #[cfg(target_os = "windows")]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetModuleHandleA(name: *const c_char) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
        }
        let module = GetModuleHandleA(c"lua51.dll".as_ptr());
        if module.is_null() {
            return std::ptr::null_mut();
        }
        GetProcAddress(module, name.as_ptr())
    }
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let read = symbol(c"luaJIT_health_read");
        if read.is_null() {
            return None;
        }
        let category = symbol(c"luaJIT_vmcategory");
        let configure = symbol(c"luaJIT_health_configure");
        let coalescing = symbol(c"luaJIT_health_coalescing");
        Some(Api {
            read: std::mem::transmute::<*mut c_void, abi::Read>(read),
            category: (!category.is_null())
                .then(|| std::mem::transmute::<*mut c_void, abi::Category>(category)),
            configure: (!configure.is_null())
                .then(|| std::mem::transmute::<*mut c_void, abi::Configure>(configure)),
            coalescing: (!coalescing.is_null())
                .then(|| std::mem::transmute::<*mut c_void, abi::Coalescing>(coalescing)),
        })
    })
    .as_ref()
}

#[derive(Default)]
struct Reader {
    state: *mut c_void,
    snapshot: Option<Snapshot>,
    coalescing_requested: bool,
    coalescing_configured: bool,
    json: CString,
}
thread_local! { static READER: RefCell<Reader> = RefCell::new(Reader::default()); }

/// LuaJIT calls this module loader with the live `lua_State*`. It installs
/// only the health reader; the allocation counter keeps its own loader.
///
/// # Safety
/// A non-null `state` must be a live LuaJIT state on its owning thread, with a
/// writable stack.
#[no_mangle]
pub unsafe extern "C" fn luaopen_stone_lua_jit_health(state: *mut crate::LuaState) -> c_int {
    if !state.is_null() {
        install(state);
    }
    0
}

/// Idempotent per VM: the health and allocator loaders both install it.
pub unsafe fn install(state: *mut crate::LuaState) {
    READER.with_borrow_mut(|r| {
        if r.state == state.cast() { return; }
        r.state = state.cast();
        r.snapshot = abi::read_snapshot(api().map(|a| a.read), r.state);
        r.coalescing_requested = std::env::var("PERF_JIT_COALESCING").as_deref() == Ok("1");
        r.coalescing_configured = false;
        if r.coalescing_requested {
            let native_api = api();
            r.coalescing_configured = r.snapshot.is_some_and(|s| s.capabilities & abi::COALESCING != 0)
                && native_api.and_then(|a| a.coalescing).is_some()
                && native_api.and_then(|a| a.configure)
                    .is_some_and(|f| f(r.state, abi::VERSION, abi::COALESCING as u32) == 1);
            if !r.coalescing_configured {
                ::log::error!(target: "STONE", "native JIT coalescing context requested but unavailable");
            }
        }
    });
}

pub fn category() -> Option<abi::Category> {
    READER.with_borrow(|r| {
        r.snapshot
            .filter(|s| s.capabilities & abi::VMCATEGORY != 0)
            .and_then(|_| api()?.category)
    })
}

/// Called on the VM thread once per outer frame; stack-owned native snapshot.
#[no_mangle]
pub extern "C" fn stone_lua_jit_health_sample() -> c_int {
    READER.with_borrow_mut(|r| {
        r.snapshot = unsafe { abi::read_snapshot(api().map(|a| a.read), r.state) };
        r.snapshot.is_some() as c_int
    })
}

/// Per-frame native JIT deltas for the profiler row. Reads the live counters,
/// diffs them against the stored baseline, writes exactly
/// `abi::FRAME_DELTA_COLUMNS` whole-number floats into caller-owned `out`,
/// and stores the live read as the new baseline, so consecutive calls delimit
/// consecutive frames. `f32` matches the frame-record row precision: counter
/// deltas are exact, and byte deltas only quantize past 2^24 per frame.
/// Returns 1 on success. When the capability is absent it zero-fills and
/// returns 0: rows stay JSON-safe, and the run-level export
/// (`available: false`) tells readers to ignore them. A null `out` or a short
/// `len` writes nothing and returns 0. No allocation on any path.
#[no_mangle]
pub extern "C" fn stone_lua_jit_health_frame_delta(out: *mut f32, len: usize) -> c_int {
    if out.is_null() || len < abi::FRAME_DELTA_COLUMNS {
        return 0;
    }
    READER.with_borrow_mut(|r| {
        let live = unsafe { abi::read_snapshot(api().map(|a| a.read), r.state) };
        let deltas = match (r.snapshot, live) {
            (Some(prev), Some(cur)) => abi::frame_deltas(&prev, &cur),
            _ => [0.0; abi::FRAME_DELTA_COLUMNS],
        };
        if live.is_some() {
            r.snapshot = live;
        }
        unsafe {
            for (i, v) in deltas.iter().enumerate() {
                out.add(i).write(*v as f32);
            }
        }
        live.is_some() as c_int
    })
}

fn coalescing_entry_json(entry: abi::CoalescingEntry) -> serde_json::Value {
    serde_json::json!({
        "sequence": entry.sequence,
        "occurrences": entry.occurrences,
        "generation": entry.generation,
        "parent_uid": entry.parent_uid,
        "start_proto": entry.start_proto,
        "failure_proto": entry.failure_proto,
        "allow_gpr": entry.allow_gpr,
        "allow_fpr": entry.allow_fpr,
        "free_gpr": entry.free_gpr,
        "free_fpr": entry.free_fpr,
        "live_gpr": entry.live_gpr,
        "live_fpr": entry.live_fpr,
        "parent": entry.parent,
        "exitno": entry.exitno,
        "subreason": entry.subreason,
        "subreason_name": abi::subreason_name(entry.subreason).unwrap_or("unknown"),
        "start_pc": entry.start_pc,
        "failure_pc": entry.failure_pc,
        "framedepth": entry.framedepth,
        "baseslot": entry.baseslot,
        "maxslot": entry.maxslot,
        "snapshot": entry.snapshot,
        "snapshot_entries": entry.snapshot_entries,
        "snapshot_slots": entry.snapshot_slots,
        "inherited": entry.inherited,
        "failed_ref": entry.failed_ref,
        "failed_slot": entry.failed_slot,
        "failed_type": entry.failed_type,
        "parent_regsp": entry.parent_regsp,
        "target_reg": entry.target_reg,
        "target_spill": entry.target_spill,
        "slot_count": entry.slot_count,
        "slots_truncated": entry.slots_truncated
    })
}

fn coalescing_json(
    state: *mut c_void,
    snapshot: Option<Snapshot>,
    configured: bool,
) -> serde_json::Value {
    let snapshot = unsafe { abi::read_snapshot(api().map(|a| a.read), state) }.or(snapshot);
    let entries = if configured {
        unsafe { abi::read_coalescing(api().and_then(|a| a.coalescing), state, 0) }
    } else {
        None
    };
    let (available, rows) = match entries {
        Some(entries) => (
            true,
            entries
                .into_iter()
                .map(coalescing_entry_json)
                .collect::<Vec<_>>(),
        ),
        None => {
            if configured {
                ::log::error!(target: "STONE", "native JIT coalescing reader failed after configuration");
            }
            (false, Vec::new())
        }
    };
    serde_json::json!({
        "available": available,
        "entries": rows,
        "ring_next": snapshot.map(|s| s.ring_next),
        "ring_overflow": snapshot.map(|s| s.ring_overflow)
    })
}

/// Export only: JSON construction stays outside the per-frame read path.
#[no_mangle]
pub extern "C" fn stone_lua_jit_health_json() -> *const c_char {
    READER.with_borrow_mut(|r| {
        let mut value = match r.snapshot {
            None => serde_json::json!({"available": false}),
            Some(s) => {
                let end = s
                    .build_id
                    .iter()
                    .position(|v| *v == 0)
                    .unwrap_or(s.build_id.len());
                serde_json::json!({
                    "available": true, "schema": s.version, "capabilities": s.capabilities,
                    "build_id": String::from_utf8_lossy(&s.build_id[..end]),
                    "basis": "vm-lifetime", "root_starts": s.root_starts,
                    "side_starts": s.side_starts, "root_compiles": s.root_compiles,
                    "side_compiles": s.side_compiles, "aborts_by_reason": s.aborts.as_slice(),
                    "flushes": s.flushes, "generation": s.generation,
                    "emitted_mcode_bytes": s.emitted_mcode_bytes,
                    "live_traces": s.live_traces, "live_mcode_bytes": s.live_mcode_bytes,
                    "arena_bytes": s.arena_bytes, "arena_occupied_bytes": s.arena_occupied_bytes,
                    "code_reserved_bytes": s.code_reserved_bytes,
                    "code_current_free_bytes": s.code_current_free_bytes,
                    "coalescing_next": s.ring_next, "coalescing_overflow": s.ring_overflow
                })
            }
        };
        if r.coalescing_requested {
            value["coalescing"] = coalescing_json(r.state, r.snapshot, r.coalescing_configured);
        }
        r.json = CString::new(value.to_string()).expect("JSON escapes NUL");
        r.json.as_ptr()
    })
}
