//! Cell, value and function names in emitted Lua.

use super::*;

/// The Lua name for a cell read/write: a coroutine frame cell renders as the
/// state-table field `<s>.c<slot>`; everything else is its colored local
/// (`c<color>`) or, when uncolored, the raw `c<raw>` fallback.
pub(crate) fn cellname(
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    colors: &[u32],
    c: Idx<Cell>,
) -> String {
    let mut s = String::new();
    write_cellname(&mut s, func, frame, colors, c);
    s
}

/// Append a cell reference (a coroutine frame field `s.c<slot>` or a plain local)
/// directly into `out` — the per-operand-allocation-free primitive `cellname`
/// wraps. A frame cell renders `<s>.c<slot>` in the `__step` body (the `s`
/// reference resolves recursively through this same fn, itself a plain local
/// there); env-record captures and box `.v` access are now materialized as MIR
/// `Field`/`StoreField` by `convert_closures`, so this fn no longer special-cases
/// them.
pub(crate) fn write_cellname(
    out: &mut String,
    func: &MirFunc,
    frame: Option<&FrameCtx>,
    colors: &[u32],
    c: Idx<Cell>,
) {
    if let Some(f) = frame {
        if f.fields.contains(&c) {
            write_cellname(out, func, None, colors, f.s_cell);
            out.push_str(".c");
            let slot = *f
                .slots
                .get(&c)
                .expect("frame field cell must carry a dense slot");
            push_uint(out, slot);
            return;
        }
    }
    write_cell_local(out, func, colors, c);
}

pub(crate) fn write_cell_local(out: &mut String, func: &MirFunc, colors: &[u32], c: Idx<Cell>) {
    out.push('c');
    match colors[raw(c) as usize] {
        u32::MAX => push_uint(out, func.emit_cell_id(c)),
        col => push_uint(out, col),
    }
}

pub(crate) fn cell_is_uint(ws: &Workspace, ty: Option<bone_core::prelude::TyId>) -> bool {
    let Some(bone_core::prelude::TyId::Static(module, index)) = ty else {
        return false;
    };
    matches!(
        ws.module(module).ty_arena[index],
        bone_core::hir::TypeExpr::Builtin(bone_core::hir::BuiltinTy::Uint)
    )
}

// --- relooper: counted-`for` recovery (structured control flow) ------------
//
// A `for i in lo..hi` / `for x in xs` / `enumerate(..)` lowers to a canonical
// 4-block counted-loop CFG: a preheader storing the counter, a header testing
// `i < hi` (or `<=`), a body, and a pure-increment latch jumping back. LuaJIT
// specializes a real numeric `for` (FORL induction) but UNROLLS a goto-loop ->
// mcode blowup -> blacklist -> interpreter spiral (docs/skull-throughput-
// investigation.md). So we pattern-match that exact shape and emit a Lua `for`,
// leaving every other terminator as goto. Recognition runs at EMIT time, on the
// post-transform CFG, so it is robust to the coroutine transform / inlining
// renumbering blocks (the design's Phase-8 "reloop over the same MIR").

/// Any source span inside `func`, for a rejection with no single offending
/// instruction. The declarations that overflow have no instruction of their
/// own; the body's first spanned instruction still points inside the Rock
/// function the user has to change.
pub(crate) fn any_inst_span(func: &MirFunc) -> Option<Span> {
    for (_, block) in func.blocks.iter() {
        for &id in &block.insts {
            if let Some(span) = func.inst_spans.get(id) {
                return Some(*span);
            }
        }
    }
    None
}

pub(crate) fn func_of(ws: &Workspace, r: FuncRef) -> &Function {
    &ws.module(r.module).functions[r.func]
}

/// The mangled Lua name for a `FuncRef` (the same name a call site emits).
pub fn mangled(ws: &Workspace, r: FuncRef) -> String {
    mangle_fn(ws, func_of(ws, r))
}

/// The Lua local name for a value: its register `r<color>` (colored values) or
/// the raw `v<raw>` fallback (an uncolored value — should not occur once every
/// value is colored, but keeps the printer total).
pub(crate) fn write_vname(out: &mut String, func: &MirFunc, colors: &[u32], id: Idx<Inst>) {
    match colors[raw(id) as usize] {
        u32::MAX => {
            out.push('v');
            push_uint(out, func.emit_inst_id(id));
        }
        c => {
            out.push('r');
            push_uint(out, c);
        }
    }
}
