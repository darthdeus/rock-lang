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
//! with reviewed exceptions for opaque bodies marked `@pure` or `@nowrite`.
//! A `for` statement
//! joins the protocol functions it desugars to.
//!
//! `@pure` is otherwise not consulted: a `@pure` Rock body is simply walked
//! and found to write nothing on its own (see `docs/purity.md`).
//!
//! The table is the least fixpoint of a monotone system over the call graph
//! (a cycle of bodies that write nothing writes nothing), computed once per
//! workspace and dropped whenever a module's inference is replaced.

use crate::hir::{
    BinOp, BodyOwner, BuiltinTy, Callee, Expr, Function, Statement, TyId, TypeCtor, TypeExpr,
};
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

    fn join(&mut self, other: &MemEffect) {
        self.lengths |= other.lengths;
        self.globals |= other.globals;
        self.elems |= other.elems;
        match (&mut self.fields, &other.fields) {
            (Some(mine), Some(theirs)) => mine.extend(theirs.iter().copied()),
            (mine, None) => *mine = None,
            (None, Some(_)) => {}
        }
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
                fns.push(match (opaque_effect(f, &callback_libraries), body, bi) {
                    (Some(effect), _, _) => (effect, Vec::new()),
                    (None, Some(body), Some(bi)) => body_effect(ws, body, bi),
                    _ => (MemEffect::all(), Vec::new()),
                });
            }
            local.push(fns);
        }
        // Pass two: close over the call graph.
        let mut table: Vec<Vec<MemEffect>> = local
            .iter()
            .map(|fns| fns.iter().map(|(own, _)| own.clone()).collect())
            .collect();
        let unknown = MemEffect::all();
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
                            .unwrap_or(&unknown);
                        joined.join(callee);
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

/// The effect of a function whose body cannot be walked, or `None` for a
/// Rock body.
fn opaque_effect(f: &Function, callback_libraries: &FxHashSet<Ustr>) -> Option<MemEffect> {
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

fn type_expr_at(ws: &Workspace, bi: &crate::workspace::BodyInference, ty: TyId) -> TypeExpr {
    let mut ty = ty;
    loop {
        let current = raw_type_expr_at(ws, bi, ty);
        if let TypeExpr::Nullable(inner) = current {
            ty = ty.with_idx(inner);
        } else {
            break current;
        }
    }
}

fn raw_type_expr_at(ws: &Workspace, bi: &crate::workspace::BodyInference, ty: TyId) -> TypeExpr {
    match ty {
        TyId::Static(mid, idx) => ws.module(mid).ty_arena[idx],
        TyId::Local(idx) => bi.ty_arena[idx],
    }
}

fn expr_type(
    ws: &Workspace,
    bi: &crate::workspace::BodyInference,
    expr: Idx<Expr>,
) -> Option<TypeExpr> {
    let &ty = bi.expr_types.get(expr)?;
    Some(type_expr_at(ws, bi, ty))
}

fn index_may_call_table_metamethod(
    ws: &Workspace,
    bi: &crate::workspace::BodyInference,
    base: Idx<Expr>,
) -> bool {
    let Some(expr) = expr_type(ws, bi, base) else {
        return true;
    };
    matches!(
        expr,
        TypeExpr::App {
            ctor: TypeCtor::Array | TypeCtor::Map,
            ..
        } | TypeExpr::Builtin(BuiltinTy::Any)
            | TypeExpr::Generic(..)
            | TypeExpr::Infer(..)
    )
}

fn field_may_call_callback(
    ws: &Workspace,
    bi: &crate::workspace::BodyInference,
    base: Idx<Expr>,
) -> bool {
    let Some(&source_ty) = bi.expr_types.get(base) else {
        return true;
    };
    let mut ty = source_ty;
    loop {
        match raw_type_expr_at(ws, bi, ty) {
            TypeExpr::Infer(var, _) => match bi.substitutions.get(var) {
                Some(next) => ty = next,
                None => return true,
            },
            TypeExpr::Nullable(inner) => ty = ty.with_idx(inner),
            TypeExpr::App {
                ctor: TypeCtor::Ref,
                params,
            } => {
                let Some(inner) = params.into_iter().next() else {
                    return true;
                };
                ty = ty.with_idx(inner);
            }
            TypeExpr::App {
                ctor: TypeCtor::Nominal(name),
                ..
            } => {
                return ws
                    .struct_by_name
                    .get(&name)
                    .is_some_and(|sid| !ws.module(sid.module).structs[sid.def].is_value);
            }
            TypeExpr::Generic(..) | TypeExpr::Builtin(BuiltinTy::Any) => return true,
            _ => return false,
        }
    }
}

fn equality_operand_may_call_metamethod(
    ws: &Workspace,
    body: &crate::hir::Body,
    bi: &crate::workspace::BodyInference,
    expr: Idx<Expr>,
) -> bool {
    if matches!(body.expr_arena[expr], Expr::Null) {
        return false;
    }
    !matches!(
        expr_type(ws, bi, expr),
        Some(TypeExpr::Builtin(
            BuiltinTy::Unit
                | BuiltinTy::Int
                | BuiltinTy::Uint
                | BuiltinTy::Float
                | BuiltinTy::F32
                | BuiltinTy::Bool
                | BuiltinTy::String
                | BuiltinTy::Never
        ))
    )
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
                    }
                    // A type's constructor and the `enumerate` template
                    // resolve to no function; neither runs user code.
                    None if is_constructor(ws, *name) || name.as_str() == "enumerate" => {}
                    None => effect.join(&MemEffect::all()),
                },
                Callee::Local(_) | Callee::Value(_) => effect.join(&MemEffect::all()),
            },
            Expr::SafeNavCall { .. } => match bi.resolved_overload_calls.get(&e) {
                Some(&target) => {
                    callees.push(target);
                }
                None => effect.join(&MemEffect::all()),
            },
            // A table lookup can enter __index. An indexed assignment can
            // enter __newindex, even when its direct store is element-only.
            Expr::FieldAccess { base, .. } | Expr::SafeNav { base, .. }
                if field_may_call_callback(ws, bi, *base) =>
            {
                effect.join(&MemEffect::all())
            }
            Expr::Index { base, .. } if index_may_call_table_metamethod(ws, bi, *base) => {
                effect.join(&MemEffect::all())
            }
            Expr::BinOp {
                op: BinOp::Eq | BinOp::NotEq,
                lhs,
                rhs,
            } if equality_operand_may_call_metamethod(ws, body, bi, *lhs)
                && equality_operand_may_call_metamethod(ws, body, bi, *rhs) =>
            {
                effect.join(&MemEffect::all())
            }
            Expr::Yield(_) | Expr::YieldDelegate(_) => effect.join(&MemEffect::all()),
            _ => {}
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
