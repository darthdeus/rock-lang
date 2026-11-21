//! Single-function entry points: plain, trampolined and closure-carrying bodies.

use super::*;

pub fn emit_fn(ws: &Workspace, mf: &MirFunc, js_name: &str) -> Result<String, CompilerError> {
    let names = CalleeNames::default();
    emit_fn_with_callees(
        ws,
        mf,
        js_name,
        &names,
        &HashSet::new(),
        &GlobalCtx::default(),
    )
}

/// [`emit_fn`] with known co-emitted callees: a call to one renders as `<js name>(args)`; anything
/// else unmapped rejects.
pub(super) fn emit_fn_with_callees(
    ws: &Workspace,
    mf: &MirFunc,
    js_name: &str,
    callees: &CalleeNames<'_>,
    hoisted: &HashSet<ustr::Ustr>,
    globals: &GlobalCtx,
) -> Result<String, CompilerError> {
    if mf.is_coroutine {
        return Err(err("JS subset: coroutine fn not supported"));
    }
    let (mut frame, unwind) = FnFrame::new(ws, mf, callees, hoisted, globals, mf.entry, false);
    let Some(shape) = placement::Shape::analyze(&frame.emitter, &frame.region) else {
        return emit_fn_trampoline(
            ws, mf, js_name, callees, hoisted, globals, mf.entry, &mf.params, true,
        );
    };
    let header = frame.header(js_name, &mf.params, true);
    placement::emit_body(&mut frame.emitter, &shape, mf.entry)?;
    Ok(format!(
        "{header}{}}}\n",
        defer_unwind_wrap(unwind, &frame.emitter.out)
    ))
}

/// A block trampoline `for(;;) switch(__bb)` for a region, the coroutine-step shape (dispatch-into-
/// loops is irreducible, so the relooper cannot apply). Case bodies are braced so `let v<N>` temps
/// stay case-local.
pub(super) fn emit_fn_trampoline(
    ws: &Workspace,
    mf: &MirFunc,
    js_name: &str,
    callees: &CalleeNames<'_>,
    hoisted: &HashSet<ustr::Ustr>,
    globals: &GlobalCtx,
    entry: Idx<Block>,
    params: &[Idx<Cell>],
    pack_multi_return: bool,
) -> Result<String, CompilerError> {
    let (frame, unwind) = FnFrame::new(ws, mf, callees, hoisted, globals, entry, pack_multi_return);
    let header = frame.header(js_name, params, false);
    let FnFrame {
        emitter: mut em,
        region,
    } = frame;
    em.push(&format!("var __bb = {};", mf.emit_block_id(entry)));
    em.push("for (;;) {");
    em.indent += 1;
    em.push("switch (__bb) {");
    em.indent += 1;
    let mut cases = region;
    cases.sort_by_key(|b| mf.emit_block_id(*b));
    for b in cases {
        em.push(&format!("case {}: {{", mf.emit_block_id(b)));
        em.indent += 1;
        em.emit_block_effects(b)?;
        em.emit_trampoline_term(b)?;
        em.indent -= 1;
        em.push("}");
    }
    em.indent -= 1;
    em.push("}");
    em.indent -= 1;
    em.push("}");
    Ok(format!(
        "{header}{}}}\n",
        defer_unwind_wrap(unwind, &em.out)
    ))
}

struct FnFrame<'a> {
    emitter: Emitter<'a>,
    region: Vec<Idx<Block>>,
}

impl<'a> FnFrame<'a> {
    fn new(
        ws: &'a Workspace,
        mf: &'a MirFunc,
        callees: &'a CalleeNames<'a>,
        hoisted: &'a HashSet<ustr::Ustr>,
        globals: &'a GlobalCtx,
        entry: Idx<Block>,
        pack_multi_return: bool,
    ) -> (Self, bool) {
        let region = skull_mir::cfg::reverse_postorder(mf, entry);
        let (mut inlinable, _, uses) =
            skull_mir::sinking::compute_inlinable(mf, &region, |_| false);
        fold_branch_loads(mf, &region, &uses, &mut inlinable);
        skull_mir::sinking::preserve_effect_order(mf, &region, &mut inlinable);
        cap_inline_depth(mf, &region, &mut inlinable);
        let use_count = uses.count;
        let hoisted_temps = cross_block_temps(mf, &region, &use_count, &inlinable);
        let unwind = mf.registers_defer_cleanup(&region);
        let emitter = Emitter {
            ws,
            mf,
            callees,
            hoisted,
            globals,
            use_count,
            inlinable,
            pack_multi_return,
            hoisted_temps,
            out: String::new(),
            indent: if unwind { 2 } else { 1 },
        };
        (Self { emitter, region }, unwind)
    }

    fn header(&self, js_name: &str, params: &[Idx<Cell>], coerce_uint: bool) -> String {
        let (ws, mf) = (self.emitter.ws, self.emitter.mf);
        let names: Vec<String> = params.iter().map(|&c| cell_name(mf, c)).collect();
        let mut header = format!("function {js_name}({}) {{\n", names.join(", "));
        if coerce_uint {
            for &c in params {
                if cell_is_uint(ws, mf.cells[c].ty) {
                    let name = cell_name(mf, c);
                    header.push_str(&format!("  {name} = {name} >>> 0;\n"));
                }
            }
        }
        let param_set: HashSet<Idx<Cell>> = params.iter().copied().collect();
        let mut declared: Vec<u32> = Vec::new();
        let mut seen = HashSet::new();
        for &b in &self.region {
            for &iid in &mf.blocks[b].insts {
                mf.insts[iid].cell_writes_each(&mut |cell| {
                    if !param_set.contains(&cell) && seen.insert(cell) {
                        declared.push(mf.emit_cell_id(cell));
                    }
                });
            }
        }
        let mut hoisted_names: Vec<u32> = self
            .emitter
            .hoisted_temps
            .iter()
            .map(|&id| mf.emit_inst_id(Idx::from_raw(RawIdx::from_u32(id))))
            .collect();
        hoisted_names.sort_unstable();
        if !declared.is_empty() || !hoisted_names.is_empty() {
            declared.sort_unstable();
            let names: Vec<String> = declared
                .iter()
                .map(|id| format!("c{id}"))
                .chain(hoisted_names.iter().map(|id| format!("v{id}")))
                .collect();
            header.push_str(&format!("  let {};\n", names.join(", ")));
        }
        header
    }
}

/// A body registering an ordinary-fn `defer` catches what escapes it: `__rock_defer_unwind` runs
/// the cleanups above the entry depth LIFO and rethrows.
fn defer_unwind_wrap(unwind: bool, body: &str) -> String {
    if !unwind {
        return body.to_string();
    }
    format!(
        "  var __rock_defer_base = __rock_defer_top;\n  try {{\n{body}  \
         }} catch (__rock_e) {{ __rock_defer_unwind(__rock_defer_base, __rock_e); }}\n"
    )
}

pub(super) fn is_runtime_global(name: &str) -> bool {
    name == skull_mir::mir::VALUE_INTEGER_CHECK
        || name == skull_mir::mir::DEFER_PUSH
        || name == skull_mir::mir::DEFER_POP
        || name == skull_mir::mir::CORO_KILL
        || name == skull_mir::mir::CORO_HOT_RELOAD
}

/// The shared mangle keyed on `(name, declared signature)`. Exempt fns keep their bare name because
/// inline(js) bodies and driver lines are verbatim text.
pub(crate) fn callee_js_name(ws: &Workspace, fr: FuncRef) -> String {
    let f = &ws.module(fr.module).functions[fr.func];
    skull_mir::mangle::mangle_fn(ws, f)
}

/// One non-fn body plus its hoisted lambdas, so a rejection drops the whole definition.
pub(super) fn emit_body_with_nested(
    ws: &Workspace,
    mf: &MirFunc,
    js_name: &str,
    names: &CalleeNames<'_>,
    hoisted: &HashSet<ustr::Ustr>,
    gctx: &GlobalCtx,
) -> Result<String, CompilerError> {
    let mut out = emit_fn_with_callees(ws, mf, js_name, names, hoisted, gctx)?;
    for nf in &mf.nested_fns {
        out.push_str(&emit_body_with_nested(
            ws,
            nf,
            nf.name.as_str(),
            names,
            hoisted,
            gctx,
        )?);
    }
    Ok(out)
}

pub(super) fn emit_ginit_with_nested(
    ws: &Workspace,
    g: &GlobalInit,
    names: &CalleeNames<'_>,
    hoisted: &HashSet<ustr::Ustr>,
    gctx: &GlobalCtx,
) -> Result<String, CompilerError> {
    emit_body_with_nested(ws, &g.mf, &g.init_name, names, hoisted, gctx)
}
