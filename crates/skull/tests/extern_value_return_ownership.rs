//! An `extern(ffi, ..)` fn returning a `value` struct must return storage it
//! OWNS.
//!
//! The wrapper for such a fn passes an out-param and used to hand back a
//! reference into it:
//!
//! ```lua
//! function f()
//!   local __out = ffi.new("S[1]")
//!   lib.f(__out)
//!   return __out[0]     -- a NON-OWNING reference into __out
//! end
//! ```
//!
//! LuaJIT does not keep the parent array alive through that reference -- it is
//! collected as soon as `__out` goes out of scope, verified with an `ffi.gc`
//! finalizer in `a_reference_into_a_collected_parent_is_use_after_free` below.
//! Every later read is then a use-after-free, and it reads plausible values right
//! up until something reuses the memory.
//!
//! Marking `Vec2` nobox (`0b1875dab`) changed allocation enough to make the reuse
//! actually happen: `rock frame bitgun --dump-state` began reporting
//! `LAST_FRAME_GFX` (a `GraphicsStats` filled through exactly this wrapper) as
//! garbage in 4 of 5 runs, and the garbage decoded to ASCII from the dump's own
//! JSON buffer (`,"sh`, `cdat`, `ader`). With the stdlib's `Vec2` boxed the same
//! binary was correct 5/5, which is what identified the trigger -- but the
//! unsoundness is in this wrapper, not in the marking.

mod common;
use common::emit_module;

/// The `value` struct must be FFI-clean for the out-param wrapper to apply.
const EXTERN_VALUE_RETURN: &str = r#"
value Stats { a: int, b: int }

extern(ffi, "stone") fn stone_probe_stats() -> Stats

fn main() {
    let s = stone_probe_stats()
    print("{s.a}")
}
"#;

#[test]
fn an_extern_value_return_does_not_hand_back_a_reference_into_its_out_param() {
    let lua = emit_module(EXTERN_VALUE_RETURN);
    let wrapper = lua
        .lines()
        .skip_while(|l| !l.starts_with("function stone_probe_stats("))
        .take_while(|l| *l != "end")
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !wrapper.is_empty(),
        "no `stone_probe_stats` wrapper was emitted:\n{lua}"
    );
    assert!(
        wrapper.contains("ffi.new(\"Stats[1]\")"),
        "the out-param shape changed; this test pins the wrapper:\n{wrapper}"
    );
    assert!(
        !wrapper.contains("return __out[0]"),
        "the wrapper returns a NON-OWNING reference into `__out`, which LuaJIT is \
         free to collect immediately -- every later read is a use-after-free:\n{wrapper}"
    );
    assert!(
        wrapper.contains("ffi.new(\"Stats\", __out[0])"),
        "the wrapper must return an OWNING copy of the out-param:\n{wrapper}"
    );
}

/// The language-level fact the fix rests on, pinned so nobody has to re-derive it:
/// LuaJIT collects the parent array while a reference into it is still held. This
/// is why the bug read correct values most of the time -- and why an
/// allocation-pattern change elsewhere could expose it.
#[test]
fn a_reference_into_a_collected_parent_is_use_after_free() {
    let probe = r#"
local ffi = require("ffi")
ffi.cdef[[ typedef struct { int a, b; } RefProbe; ]]

local freed_while_referenced = false
local function ref_into_out()
    local out = ffi.new("RefProbe[1]")
    ffi.gc(out, function() freed_while_referenced = true end)
    out[0].a = 7
    return out[0]
end

local function owning_copy()
    local out = ffi.new("RefProbe[1]")
    ffi.gc(out, function() end)
    out[0].a = 7
    return ffi.new("RefProbe", out[0])
end

local ref = ref_into_out()
collectgarbage("collect")
collectgarbage("collect")
io.write(tostring(freed_while_referenced), " ", tostring(ref ~= nil), " ", tostring(owning_copy().a))
"#;
    assert_eq!(
        common::run_luajit(probe, "extern_value_return_refprobe"),
        "true true 7",
        "LuaJIT no longer collects a referenced parent -- if this flipped, re-read \
         whether the owning copy is still required"
    );
}
