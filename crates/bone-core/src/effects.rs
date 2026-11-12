//! Memory effects of a call, inferred over the whole program: which places a
//! callee may write, so an optimization that holds a fact about a place can
//! keep it across calls that provably leave the place alone.
//!
//! The effect is a conservative upper bound over the callee and everything
//! it can reach. A Rock body contributes what it writes itself (a field
//! assignment names its field; an index assignment writes an element; a
//! global assignment writes a global) joined with the effects of every call
//! it makes. Anything whose body cannot be read -- an `inline(lua)`, `extern`
//! or `operator(lua)` body, a call through a function value, a yield (the
//! resumer runs), a call the inference left unresolved -- writes everything,
//! with two reviewed exceptions in the standard library: an opaque body
//! marked `@pure` and `print` write nothing, and the push bodies write only
//! the `len` field (they grow a vector, never shrink one). A `for` statement
//! joins the protocol functions it desugars to.
//!
//! `@pure` is otherwise not consulted: a `@pure` Rock body is simply walked
//! and found to write nothing on its own (see `docs/purity.md`).
//!
//! The table is the least fixpoint of a monotone system over the call graph
//! (a cycle of bodies that write nothing writes nothing), computed once per
//! workspace and dropped whenever a module's inference is replaced.

use crate::hir::{BodyOwner, BuiltinTy, Callee, Expr, Function, Statement, TyId, TypeExpr};
use crate::prelude::*;
use crate::workspace::{BoundNext, BoundTree, Workspace};

/// What a call may write. Every field is an upper bound.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemEffect {
    /// A vector may have shrunk, or a `len` field was written other than by
    /// a push.
    pub lengths: bool,
    /// Field names that may be written, or `None` for any field.
    pub fields: Option<FxHashSet<Ustr>>,
    /// A global may be reassigned.
    pub globals: bool,
    /// An element of a container may be written.
    pub elems: bool,
}

impl MemEffect {
    /// Writes nothing reachable from the caller.
    pub fn none() -> MemEffect {
        MemEffect {
            lengths: false,
            fields: Some(FxHashSet::default()),
            globals: false,
            elems: false,
        }
    }

    /// May write anything.
    pub fn all() -> MemEffect {
        MemEffect {
            lengths: true,
            fields: None,
            globals: true,
            elems: true,
        }
    }

    pub fn is_none(&self) -> bool {
        *self == MemEffect::none()
    }

    pub fn is_all(&self) -> bool {
        self.lengths && self.fields.is_none() && self.globals && self.elems
    }

    /// Whether `field` may be written.
    pub fn writes_field(&self, field: Ustr) -> bool {
        self.fields.as_ref().is_none_or(|set| set.contains(&field))
    }

    fn join(&mut self, other: &MemEffect) -> bool {
        let before = self.clone();
        self.lengths |= other.lengths;
        self.globals |= other.globals;
        self.elems |= other.elems;
        match (&mut self.fields, &other.fields) {
            (Some(mine), Some(theirs)) => mine.extend(theirs.iter().copied()),
            (mine, None) => *mine = None,
            (None, Some(_)) => {}
        }
        *self != before
    }

    fn with_field(field: Ustr) -> MemEffect {
        let mut fields = FxHashSet::default();
        fields.insert(field);
        MemEffect {
            lengths: field.as_str() == "len",
            fields: Some(fields),
            ..MemEffect::none()
        }
    }
}

/// Per-function memory effects for a workspace.
#[derive(Clone, Debug, Default)]
pub struct EffectTable {
    by_module: Vec<Vec<MemEffect>>,
}

impl EffectTable {
    /// The effect of calling `(mid, fid)`; everything, for a function the
    /// table does not know.
    pub fn of(&self, mid: ModuleId, fid: Idx<Function>) -> MemEffect {
        self.by_module
            .get(mid.index())
            .and_then(|fns| fns.get(fid.into_raw().into_u32() as usize))
            .cloned()
            .unwrap_or_else(MemEffect::all)
    }

    pub fn compute(ws: &Workspace) -> EffectTable {
        // A callback registered with one binding can run through another
        // binding in the same library, even when that call takes no callback.
        let callback_libraries: FxHashSet<Ustr> = ws
            .modules
            .iter()
            .flat_map(|module| module.functions.iter())
            .filter_map(|(_, f)| {
                f.ffi_lib
                    .filter(|_| f.param_decls.iter().any(|p| is_fn_typed(ws, p.ty)))
            })
            .collect();
        // Pass one: each body's own writes and its resolved callees.
        let mut local: Vec<Vec<(MemEffect, Vec<(ModuleId, Idx<Function>)>)>> =
            Vec::with_capacity(ws.modules.len());
        for module in &ws.modules {
            let inference = ws.inference(module.id);
            let mut fns = Vec::with_capacity(module.functions.len());
            for (fid, f) in module.functions.iter() {
                let bi = inference.and_then(|inf| inf.per_fn.get(&BodyOwner::Function(fid)));
                let body = module.bodies.get(&BodyOwner::Function(fid));
                fns.push(
                    match (
                        opaque_effect(ws, module.id, f, &callback_libraries),
                        body,
                        bi,
                    ) {
                        (Some(effect), _, _) => (effect, Vec::new()),
                        (None, Some(body), Some(bi)) => body_effect(ws, body, bi),
                        _ => (MemEffect::all(), Vec::new()),
                    },
                );
            }
            local.push(fns);
        }
        // Pass two: close over the call graph.
        let mut table: Vec<Vec<MemEffect>> = local
            .iter()
            .map(|fns| fns.iter().map(|(own, _)| own.clone()).collect())
            .collect();
        loop {
            let mut changed = false;
            for (m, fns) in local.iter().enumerate() {
                for (i, (_, callees)) in fns.iter().enumerate() {
                    if table[m][i].is_all() {
                        continue;
                    }
                    let mut joined = table[m][i].clone();
                    for &(cm, cf) in callees {
                        let callee = table
                            .get(cm.index())
                            .and_then(|fns| fns.get(cf.into_raw().into_u32() as usize))
                            .cloned()
                            .unwrap_or_else(MemEffect::all);
                        joined.join(&callee);
                        if joined.is_all() {
                            break;
                        }
                    }
                    if joined != table[m][i] {
                        table[m][i] = joined;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        EffectTable { by_module: table }
    }
}

/// The push bodies, line by line: the one opaque mutation that only grows.
const PUSH_BODY: [&str; 2] = ["vec.len = vec.len + 1", "vec._data[vec.len] = item"];

/// The effect of a function whose body cannot be walked, or `None` for a
/// Rock body.
fn opaque_effect(
    ws: &Workspace,
    mid: ModuleId,
    f: &Function,
    callback_libraries: &FxHashSet<Ustr>,
) -> Option<MemEffect> {
    if !f.is_extern && f.inline_body.is_none() {
        return None;
    }
    // A C function reached through the FFI cannot touch a Lua table or a JS
    // object: it sees only the scalars and the native memory it is handed.
    // NativeBuffer elements alias that native memory, so their value facts
    // must be killed. A library with callback bindings may also mutate Lua.
    if f.is_extern
        && let Some(lib) = f.ffi_lib
    {
        return Some(if callback_libraries.contains(&lib) {
            MemEffect::all()
        } else {
            MemEffect {
                elems: true,
                ..MemEffect::none()
            }
        });
    }
    // `@nowrite`: the body stores to no table field, element or global the
    // program can reach, including NativeBuffer storage (IO, allocation and
    // raising are fine).
    // `@writes_elems`: it stores only into elements of containers it is
    // handed. Both are promises about raw target code that nothing but
    // review can check, exactly like the body itself; `@pure` on an opaque
    // body is the same promise and more.
    let has = |name: &str| f.annotations.iter().any(|a| a.is_builtin(name));
    if has("pure") || has("nowrite") {
        return Some(MemEffect::none());
    }
    if has("writes_elems") {
        return Some(MemEffect {
            elems: true,
            ..MemEffect::none()
        });
    }
    let stdlib = crate::modules::is_stdlib_module_file(ws.module(mid).file.as_str());
    let push = stdlib
        && matches!(f.name.as_str(), "push" | "vec_push")
        && f.inline_body.as_ref().is_some_and(|(_, body)| {
            body.as_str()
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .eq(PUSH_BODY)
        });
    if push {
        let mut effect = MemEffect::with_field(Ustr::from("len"));
        effect.lengths = false;
        return Some(effect);
    }
    Some(MemEffect::all())
}

fn is_fn_typed(ws: &Workspace, ty: TyId) -> bool {
    let TyId::Static(mid, idx) = ty else {
        return true;
    };
    match ws.module(mid).ty_arena[idx] {
        TypeExpr::Function { .. } => true,
        TypeExpr::Builtin(BuiltinTy::Any) => true,
        TypeExpr::Nullable(inner) => is_fn_typed(ws, TyId::Static(mid, inner)),
        _ => false,
    }
}

/// A Rock body's own writes and the callees it reaches. Lambda bodies are
/// walked with the rest: a lambda that writes makes its creator write,
/// which is conservative.
/// A struct, enum or newtype name used as a call: a constructor.
fn is_constructor(ws: &Workspace, name: Ustr) -> bool {
    let bare = name.as_str().rsplit("::").next().unwrap_or_default();
    let bare = Ustr::from(bare);
    ws.struct_by_name.contains_key(&bare)
        || ws.enums_by_name.contains_key(&bare)
        || ws.newtypes_by_name.contains_key(&bare)
}

fn body_effect(
    ws: &Workspace,
    body: &crate::hir::Body,
    bi: &crate::workspace::BodyInference,
) -> (MemEffect, Vec<(ModuleId, Idx<Function>)>) {
    let mut effect = MemEffect::none();
    let mut callees = Vec::new();
    for (e, expr) in body.expr_arena.iter() {
        match expr {
            Expr::Assign { target, .. }
            | Expr::CompoundAssign { target, .. }
            | Expr::IncDec { target, .. } => effect.join(&write_effect(body, *target)),
            Expr::Call { callee, .. } => match callee {
                Callee::Unresolved { name, .. } => match bi.resolved_overload_calls.get(&e) {
                    Some(&target) => {
                        callees.push(target);
                        false
                    }
                    // A type's constructor and the `enumerate` template
                    // resolve to no function; neither runs user code.
                    None if is_constructor(ws, *name) || name.as_str() == "enumerate" => false,
                    None => effect.join(&MemEffect::all()),
                },
                Callee::Local(_) | Callee::Value(_) => effect.join(&MemEffect::all()),
            },
            Expr::SafeNavCall { .. } => match bi.resolved_overload_calls.get(&e) {
                Some(&target) => {
                    callees.push(target);
                    false
                }
                None => effect.join(&MemEffect::all()),
            },
            Expr::Yield(_) | Expr::YieldDelegate(_) => effect.join(&MemEffect::all()),
            _ => false,
        };
        if effect.is_all() {
            return (effect, Vec::new());
        }
    }
    for (_, stmt) in body.statement_arena.iter() {
        if let Statement::For { iter, .. } = stmt {
            match bi.for_iter_protocol.get(iter) {
                Some(info) => {
                    if let Some(witness) = &info.iter {
                        callees.push(witness.func);
                    }
                    collect_next(&info.next, &mut callees, &mut effect);
                }
                // A range loop desugars to no call.
                None => {}
            }
        }
        if effect.is_all() {
            return (effect, Vec::new());
        }
    }
    (effect, callees)
}

fn collect_next(
    next: &BoundNext,
    callees: &mut Vec<(ModuleId, Idx<Function>)>,
    effect: &mut MemEffect,
) {
    match next {
        BoundNext::Concrete { func, sub, .. } => {
            callees.push(*func);
            collect_tree(sub, callees, effect);
        }
        // Resolved only at an inlining root: unknown here.
        BoundNext::CallerGeneric(_) => {
            effect.join(&MemEffect::all());
        }
    }
}

fn collect_tree(
    tree: &BoundTree,
    callees: &mut Vec<(ModuleId, Idx<Function>)>,
    effect: &mut MemEffect,
) {
    for next in tree.next.values() {
        collect_next(next, callees, effect);
    }
}

/// What assigning to `target` writes. A local binding is the caller's own
/// frame and writes nothing another body can see.
fn write_effect(body: &crate::hir::Body, target: Idx<Expr>) -> MemEffect {
    match body.expr_arena[target] {
        Expr::Var(_) => MemEffect::none(),
        Expr::FieldAccess { field, .. } | Expr::SafeNav { field, .. } => {
            MemEffect::with_field(field)
        }
        Expr::Global(_) => MemEffect {
            globals: true,
            ..MemEffect::none()
        },
        Expr::Index { .. } => MemEffect {
            elems: true,
            ..MemEffect::none()
        },
        _ => MemEffect::all(),
    }
}
