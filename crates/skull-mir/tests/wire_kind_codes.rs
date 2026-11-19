//! `WireKind::code()` is a compiler-to-host ABI: the host reads it to pick the
//! component count and the float or int view, and the engine spells the same
//! numbers as `WIRE_KIND_*` globals.

use skull_mir::gpu_layout::WireKind;

#[test]
fn the_codes_are_the_documented_numbers() {
    let codes = [
        (WireKind::F32, 0),
        (WireKind::F32x2, 1),
        (WireKind::F32x3, 2),
        (WireKind::F32x4, 3),
        (WireKind::I32, 4),
        (WireKind::Bool32, 5),
        (WireKind::Mat4, 6),
        (WireKind::U32, 7),
    ];
    for (kind, code) in codes {
        assert_eq!(kind.code(), code, "{kind:?}");
    }
}
