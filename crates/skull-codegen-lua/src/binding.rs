use super::*;

/// The callees one body binds as immutable upvalues, and the rank its binding
/// line must sort at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalleeBindingPlan {
    pub name: String,
    pub factory: String,
    /// Global and factory-local names, sorted by global name.
    pub captures: Vec<(ustr::Ustr, ustr::Ustr)>,
    /// Emission order for the binding pass: the caller's SCC number, strictly
    /// greater than every component it captures from.
    pub rank: u32,
}

impl CalleeBindingPlan {
    pub(crate) fn parameter_for(&self, name: &str) -> Option<ustr::Ustr> {
        self.captures
            .binary_search_by(|(global, _)| global.as_str().cmp(name))
            .ok()
            .map(|i| self.captures[i].1)
    }

    /// The second-pass line: `F = __rock_mk_F(a, b)`.
    pub fn write_bind_line(&self, out: &mut String) {
        out.push_str(&self.name);
        out.push_str(" = ");
        out.push_str(&self.factory);
        out.push('(');
        for (i, (global, _)) in self.captures.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(global.as_str());
        }
        out.push_str(")\n");
    }
}

/// Bind final, compiler-owned callees as immutable upvalues. On GC64 they
/// cost two trace constant slots instead of five plus a global-value guard.
///
/// Recursive, unknown, rebindable and non-emitted callees stay global.
/// Runtime uint validators are already available when binding begins.
pub fn callee_binding_plan(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    self_ref: FuncRef,
    func: &MirFunc,
) -> Option<CalleeBindingPlan> {
    let rank = ctx.scc_of(self_ref);
    let mut captures: Vec<ustr::Ustr> = Vec::new();
    let mut touches_state = false;
    // Free names in spliced inline(lua) text resolve lexically, so an alias
    // spelled like one would capture it.
    let mut raw_names: Vec<&str> = Vec::new();
    let mut raw_callees: Vec<FuncRef> = Vec::new();
    // Cleanup unlinks instructions without freeing arena slots. Match the
    // emitted body's CFG so discarded calls and separate scopes use no slots.
    let blocks = reverse_postorder(func, func.entry());
    for id in blocks
        .iter()
        .flat_map(|bid| func.blocks[*bid].insts.iter().copied())
    {
        let inst = &func.insts[id];
        touches_state |= matches!(inst, Inst::Global(_) | Inst::StoreGlobal { .. });
        match inst {
            Inst::IntToUint(x) | Inst::FloatToUint(x)
                if checked_u32_constant(func, *x).is_none() =>
            {
                captures.push(skull_mir::names::ustr(CHECKED_U32));
            }
            Inst::UintShl(_, count) | Inst::UintShr(_, count)
                if !valid_constant_shift_count(func, *count) =>
            {
                captures.push(skull_mir::names::ustr(U32_SHIFT_COUNT));
            }
            Inst::Format { .. } | Inst::FormatDyn { .. }
                if crate::value::fixed_point_format(func, inst) =>
            {
                captures.push(skull_mir::names::ustr(FIXED_POINT));
            }
            _ => {}
        }
        let callee = match inst {
            Inst::Call { func, .. } | Inst::CallMulti { func, .. } => *func,
            _ => continue,
        };
        if let Some((_, text)) = &func_of(ws, callee).inline_body
            && !raw_callees.contains(&callee)
        {
            raw_callees.push(callee);
            raw_names.extend(alias_shaped_words(text.as_str()));
        }
        // Ranks remain fixed through body edits. New backedges must stay
        // global so the retained factory order remains acyclic.
        match (rank, ctx.scc_of(callee)) {
            (Some(ours), Some(theirs)) if theirs < ours => {}
            _ => continue,
        }
        let decl = func_of(ws, callee);
        if decl.is_extern || skull_mir::mangle::is_exempt(decl) || decl.inline_body.is_some() {
            continue;
        }
        if decl.has_iterator_bounds() {
            continue;
        }
        if !decl.generics.is_empty() && !func.spec_bindings.contains_key(&id) {
            continue;
        }
        let symbol = match func.spec_bindings.get(&id) {
            Some(inst) => ctx.instance_symbol(ws, interner, inst),
            None => ctx.mangled_name(ws, callee),
        };
        // A body that calls its own emitted name (a specialized instance
        // recursing through the shared declaration) would capture itself.
        if symbol.as_str() == name {
            continue;
        }
        captures.push(symbol);
    }
    captures.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    captures.dedup();
    if captures.is_empty() {
        return None;
    }
    // A body that touches a mutable global captures the storage cell as an
    // upvalue too, against the same 60-upvalue limit the callees count toward.
    let reserved = usize::from(touches_state);
    let limit = MAX_BOUND_CALLEES - reserved;
    captures.truncate(limit);
    // Lua derives traceback names from call-site upvalues. Preserve them at
    // unwind boundaries so the captured raise-site trace names the callee.
    let preserve_names = func.registers_defer_cleanup(&blocks);
    let mut next_alias = 0;
    let captures = captures
        .into_iter()
        .map(|capture| {
            // Runtime helper calls are printed directly, so their captures
            // retain the original spelling.
            if preserve_names
                || matches!(
                    capture.as_str(),
                    CHECKED_U32 | U32_SHIFT_COUNT | FIXED_POINT
                )
            {
                return (capture, capture);
            }
            loop {
                let alias = skull_mir::names::indexed("b", "", next_alias);
                next_alias += 1;
                if !ctx.reserved.contains_key(alias.as_str())
                    && !raw_names.contains(&alias.as_str())
                {
                    break (capture, alias);
                }
            }
        })
        .collect();
    Some(CalleeBindingPlan {
        factory: ctx.mint_synthetic(format!("{CALLEE_FACTORY_PREFIX}{name}")),
        name: name.to_string(),
        captures,
        // Runtime validators have no body dependencies, including for a leaf
        // function absent from the call-edge inventory.
        rank: rank.unwrap_or(0),
    })
}

const CHECKED_U32: &str = "__rock_checked_u32";
const U32_SHIFT_COUNT: &str = "__rock_u32_shift_count";
pub(crate) const FIXED_POINT: &str = "__rock_fixed_point";

/// The words of a Lua text spelled like a factory alias (`b<N>`), strings and
/// comments included: a name to avoid needs no exact tokenizer.
fn alias_shaped_words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|word| {
            word.strip_prefix('b')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
}
