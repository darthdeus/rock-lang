//! Bound-callee plans: which callees a body captures as upvalues.

use super::*;

/// The callees one body binds as immutable upvalues, and the rank its binding
/// line must sort at. Built by [`callee_binding_plan`]; consumed by the emitter
/// (which names the factory parameters) and by the assembler (which emits the
/// second pass).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalleeBindingPlan {
    /// The bound function's emitted Lua name.
    pub name: String,
    /// Its factory's name, minted through [`CodegenCtx::mint_synthetic`].
    pub factory: String,
    /// The callee names to capture, sorted, deduplicated, and truncated to
    /// [`MAX_BOUND_CALLEES`]. These are the globals read at bind time.
    pub captures: Vec<String>,
    /// Factory-local names in capture order. Global binding names stay stable.
    pub parameters: Vec<ustr::Ustr>,
    /// Ascending emission order for the binding pass: the caller's strongly
    /// connected component number, which is strictly greater than every
    /// component it captures from (see [`call_sccs`]).
    pub rank: u32,
    /// True when this body had more bindable callees than [`MAX_BOUND_CALLEES`],
    /// so the tail stayed global lookups.
    pub capped: bool,
}

impl CalleeBindingPlan {
    pub(crate) fn parameter_for(&self, name: &str) -> Option<ustr::Ustr> {
        self.captures
            .binary_search_by(|capture| capture.as_str().cmp(name))
            .ok()
            .map(|i| self.parameters[i])
    }

    /// The second-pass line: `F = __rock_mk_F(a, b)`.
    pub fn bind_line(&self) -> String {
        let mut out = String::with_capacity(self.name.len() + self.factory.len() + 8);
        out.push_str(&self.name);
        out.push_str(" = ");
        out.push_str(&self.factory);
        out.push('(');
        for (i, c) in self.captures.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(c);
        }
        out.push_str(")\n");
        out
    }
}

/// The binding plan for `func`, emitted under `name` for declaration
/// `self_ref`, or `None` when nothing is bound (every call in the body is then
/// a global lookup).
///
/// A global lookup (`GGET`) costs a trace 5 constant-pool slots per distinct
/// callee on GC64 -- the name string (a GC constant, 2 slots), its `KSLOT`
/// hash slot, and the resolved function object (2 more) -- plus the guard that
/// the global still holds that function. LuaJIT's `maxirconst` limit is 500
/// SLOTS, and "trace too long" (the limit being hit) can dominate trace
/// aborts. An IMMUTABLE upvalue costs 2: only the function object, folded into
/// the trace with no lookup and no guard.
///
/// A callee is bound only when the emitter can PROVE it is an ordinary emitted
/// Lua function whose global is final by the time this body's binding line
/// runs. Everything else stays a global lookup:
///
/// - a callee in the caller's own strongly connected component (including
///   direct self-recursion): binding it would need its final closure before it
///   exists, and the condensation rank cannot separate them;
/// - a callee, or a caller, absent from the call graph the context was given:
///   with no component number there is no proof of either fact;
/// - an EXEMPT declaration (`extern`, `@nomangle`, `@test`, `main`, `test_*`):
///   its bare global is a published name anything may rebind, and hot reload
///   and the engine both look those up by name;
/// - an `inline(lua)` body: the definition is hand-written text, and a splice
///   may remove the call entirely;
/// - an unspecialized generic or an `Iterator`-bounded declaration: neither has
///   a standalone definition of its own.
///
/// A call THROUGH a function value (`CallValue`) has no statically known callee
/// and is never bound.
/// Runtime uint validators are initialized by the prelude and can be bound
/// even when the caller has no edges in the call graph.
pub fn callee_binding_plan(
    ws: &Workspace,
    ctx: &CodegenCtx,
    interner: &MonoInterner,
    name: &str,
    self_ref: FuncRef,
    func: &MirFunc,
) -> Option<CalleeBindingPlan> {
    let rank = ctx.scc_of(self_ref);
    let mut captures: Vec<String> = Vec::new();
    let mut touches_state = false;
    let mut has_inline_body = false;
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
                captures.push("__rock_checked_u32".to_string());
            }
            Inst::UintShl(..) | Inst::UintShr(..) => {
                captures.push("__rock_u32_shift_count".to_string());
            }
            _ => {}
        }
        let callee = match inst {
            Inst::Call { func, .. } | Inst::CallMulti { func, .. } => *func,
            _ => continue,
        };
        has_inline_body |= func_of(ws, callee).inline_body.is_some();
        // Ranks remain fixed through body edits. New backedges must stay
        // global so the retained factory order remains acyclic.
        match (rank, ctx.scc_of(callee)) {
            (Some(ours), Some(theirs)) if theirs < ours => {}
            _ => continue,
        }
        let decl = func_of(ws, callee);
        if skull_mir::mangle::is_exempt(decl) || decl.inline_body.is_some() {
            continue;
        }
        if decl.generic_bounds.iter().any(Option::is_some) {
            continue;
        }
        if !decl.generics.is_empty() && !func.spec_bindings.contains_key(&id) {
            continue;
        }
        let symbol = match func.spec_bindings.get(&id) {
            Some(inst) => inst.symbol(ws, interner),
            None => ctx.mangled_name(ws, callee).to_string(),
        };
        // A body that calls its own emitted name (a specialized instance
        // recursing through the shared declaration) would capture itself.
        if symbol == name {
            continue;
        }
        captures.push(symbol);
    }
    captures.sort_unstable();
    captures.dedup();
    if captures.is_empty() {
        return None;
    }
    // A body that touches a mutable global captures the storage cell as an
    // upvalue too, against the same 60-upvalue limit the callees count toward.
    let reserved = usize::from(touches_state);
    let limit = MAX_BOUND_CALLEES - reserved;
    let capped = captures.len() > limit;
    captures.truncate(limit);
    // Lua derives traceback names from call-site upvalues. Preserve them at
    // unwind boundaries so the captured raise-site trace names the callee.
    let preserve_names = has_inline_body || func.registers_defer_cleanup(&blocks);
    let mut next_alias = 0;
    let parameters = captures
        .iter()
        .map(|capture| {
            // Raw Lua may refer to any lexical name. Runtime helper calls are
            // printed directly, so their captures retain the original spelling.
            if preserve_names
                || matches!(
                    capture.as_str(),
                    "__rock_checked_u32" | "__rock_u32_shift_count"
                )
            {
                return ustr::Ustr::from(capture);
            }
            loop {
                let alias = format!("b{next_alias}");
                next_alias += 1;
                if !ctx.reserved.contains_key(&alias) {
                    break ustr::Ustr::from(&alias);
                }
            }
        })
        .collect();
    Some(CalleeBindingPlan {
        factory: ctx.mint_synthetic(format!("{CALLEE_FACTORY_PREFIX}{name}")),
        name: name.to_string(),
        captures,
        parameters,
        // Runtime validators have no body dependencies, including for a leaf
        // function absent from the call-edge inventory.
        rank: rank.unwrap_or(0),
        capped,
    })
}
