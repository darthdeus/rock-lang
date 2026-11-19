use super::*;

/// A frame cell renders as `<s>.c<slot>`; others as `c<color>`, or `c<raw>`
/// when uncolored.
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
    let number = match colors[raw(c) as usize] {
        u32::MAX => func.emit_cell_id(c),
        col => col,
    };
    scope_guard::cell(func, c, number);
    push_uint(out, number);
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

/// Any source span inside `func`, for a rejection with no single offending
/// instruction.
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

pub(crate) fn write_vname(out: &mut String, func: &MirFunc, colors: &[u32], id: Idx<Inst>) {
    match colors[raw(id) as usize] {
        u32::MAX => {
            out.push('v');
            push_uint(out, func.emit_inst_id(id));
        }
        c => {
            scope_guard::register(func, c);
            out.push('r');
            push_uint(out, c);
        }
    }
}
