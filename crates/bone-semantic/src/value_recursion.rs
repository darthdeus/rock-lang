//! Recursive value layouts are rejected where they are defined.
//!
//! A `value` is stored inline wherever it is held, so a value that contains
//! itself by value -- directly, through other values, newtypes, tuples or a
//! stored type argument -- has no finite layout. A nullable link does not
//! help: `T?` holds the whole `T` plus a presence bit. The rule is about the
//! declaration, not the use, so it holds on every backend and in the LSP.
//!
//! Vec, Map, `ref`, functions, `any`, enums and identity structs hold what
//! they name by reference, and a phantom type argument (`Entity<T>`) holds
//! nothing, so recursion through any of them is admitted.

use crate::hir::{TypeCtor, TypeExpr, VarId};
use crate::prelude::*;

/// A by-value nominal: a `value` struct or a newtype.
struct Nominal {
    module: ModuleId,
    display: Ustr,
    /// The declared generic params, as `Generic` var ids, in order.
    params: Vec<VarId>,
    /// The types this nominal holds by value, with the span to report at.
    members: Vec<(TyId, Span)>,
}

/// One by-value containment step out of a member type: the nominal reached,
/// and the outermost type that reached it when that differs (`Box<Node>`
/// reaching `Node` through a stored argument).
struct Edge {
    target: Ustr,
    via: Option<String>,
    span: Span,
}

pub(crate) fn check_value_recursion(ws: &mut Workspace, fresh: &[ModuleId]) {
    let nominals = collect_nominals(ws);
    let stored = stored_params(ws, &nominals);

    let mut edges: FxHashMap<Ustr, Vec<Edge>> = FxHashMap::default();
    for (&key, nominal) in nominals.iter() {
        let out = edges.entry(key).or_default();
        for &(ty, span) in &nominal.members {
            by_value_targets(ws, &nominals, &stored, ty, &mut |target, via| {
                out.push(Edge { target, via, span });
            });
        }
    }

    let mut errs: Vec<(ModuleId, CompilerError)> = Vec::new();
    let mut keys: Vec<Ustr> = nominals.keys().copied().collect();
    keys.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    for key in keys {
        let nominal = &nominals[&key];
        let mut reported: FxHashSet<Span> = FxHashSet::default();
        for edge in edges.get(&key).into_iter().flatten() {
            if reported.contains(&edge.span) {
                continue;
            }
            let Some(rest) = path_back(&edges, edge.target, key) else {
                continue;
            };
            reported.insert(edge.span);
            let mut steps = vec![nominal.display.to_string()];
            if let Some(via) = &edge.via {
                steps.push(via.clone());
            }
            for step in rest {
                let n = &nominals[&step.0];
                if let Some(via) = step.1 {
                    steps.push(via);
                }
                steps.push(n.display.to_string());
            }
            errs.push((
                nominal.module,
                CompilerError::new_with_label(
                    edge.span,
                    format!(
                        "recursive value type: {}; use a reference or handle",
                        steps.join(" -> ")
                    ),
                ),
            ));
        }
    }

    for (mid, e) in errs {
        if fresh.contains(&mid) {
            ws.module_mut(mid).interface_errors.push(e);
        }
    }
}

fn collect_nominals(ws: &Workspace) -> FxHashMap<Ustr, Nominal> {
    let mut out = FxHashMap::default();
    for (&key, sid) in ws.struct_by_name.iter() {
        let module = ws.module(sid.module);
        let def = &module.structs[sid.def];
        if !def.is_value {
            continue;
        }
        out.insert(
            key,
            Nominal {
                module: sid.module,
                display: def.name,
                params: generic_vars(ws, &def.generics),
                members: def.fields.iter().map(|f| (f.ty, f.span)).collect(),
            },
        );
    }
    for (&key, nid) in ws.newtypes_by_name.iter() {
        let module = ws.module(nid.module);
        let def = &module.newtypes[nid.def];
        out.insert(
            key,
            Nominal {
                module: nid.module,
                display: def.name,
                params: generic_vars(ws, &def.generics),
                members: vec![(def.wrapped, def.span)],
            },
        );
    }
    out
}

fn generic_vars(ws: &Workspace, generics: &[TyId]) -> Vec<VarId> {
    generics
        .iter()
        .filter_map(|&g| match static_expr(ws, g) {
            Some((_, TypeExpr::Generic(var, _))) => Some(var),
            _ => None,
        })
        .collect()
}

fn static_expr(ws: &Workspace, ty: TyId) -> Option<(ModuleId, TypeExpr)> {
    match ty {
        TyId::Static(mid, idx) => Some((mid, ws.module(mid).ty_arena[idx])),
        TyId::Local(_) => None,
    }
}

/// Per nominal, which of its type params it holds by value, to a fixpoint:
/// `Box<T> { t: T }` stores `T`; `Entity<T>` stores nothing.
fn stored_params(
    ws: &Workspace,
    nominals: &FxHashMap<Ustr, Nominal>,
) -> FxHashMap<Ustr, FxHashSet<usize>> {
    let mut stored: FxHashMap<Ustr, FxHashSet<usize>> = FxHashMap::default();
    loop {
        let mut changed = false;
        for (&key, nominal) in nominals.iter() {
            if nominal.params.is_empty() {
                continue;
            }
            let mut vars = Vec::new();
            for &(ty, _) in &nominal.members {
                by_value_params(ws, nominals, &stored, ty, &mut vars);
            }
            for var in vars {
                if let Some(i) = nominal.params.iter().position(|&p| p == var) {
                    changed |= stored.entry(key).or_default().insert(i);
                }
            }
        }
        if !changed {
            return stored;
        }
    }
}

/// The type params `ty` holds by value.
fn by_value_params(
    ws: &Workspace,
    nominals: &FxHashMap<Ustr, Nominal>,
    stored: &FxHashMap<Ustr, FxHashSet<usize>>,
    ty: TyId,
    out: &mut Vec<VarId>,
) {
    let Some((mid, te)) = static_expr(ws, ty) else {
        return;
    };
    match te {
        TypeExpr::Generic(var, _) => out.push(var),
        TypeExpr::Nullable(inner) => {
            by_value_params(ws, nominals, stored, TyId::Static(mid, inner), out)
        }
        TypeExpr::Tuple { elements } => {
            for idx in elements {
                by_value_params(ws, nominals, stored, TyId::Static(mid, idx), out);
            }
        }
        TypeExpr::App {
            ctor: TypeCtor::Nominal(n),
            params,
        } if nominals.contains_key(&n) => {
            let Some(held) = stored.get(&n) else { return };
            for (i, idx) in params.into_iter().enumerate() {
                if held.contains(&i) {
                    by_value_params(ws, nominals, stored, TyId::Static(mid, idx), out);
                }
            }
        }
        _ => {}
    }
}

/// Every by-value nominal `ty` contains, each with the outermost type that
/// reached it when that is not the nominal itself.
fn by_value_targets(
    ws: &Workspace,
    nominals: &FxHashMap<Ustr, Nominal>,
    stored: &FxHashMap<Ustr, FxHashSet<usize>>,
    ty: TyId,
    sink: &mut dyn FnMut(Ustr, Option<String>),
) {
    fn walk(
        ws: &Workspace,
        nominals: &FxHashMap<Ustr, Nominal>,
        stored: &FxHashMap<Ustr, FxHashSet<usize>>,
        ty: TyId,
        via: Option<&str>,
        sink: &mut dyn FnMut(Ustr, Option<String>),
    ) {
        let Some((mid, te)) = static_expr(ws, ty) else {
            return;
        };
        match te {
            TypeExpr::Nullable(inner) => {
                walk(ws, nominals, stored, TyId::Static(mid, inner), via, sink)
            }
            TypeExpr::Tuple { elements } => {
                for idx in elements {
                    walk(ws, nominals, stored, TyId::Static(mid, idx), via, sink);
                }
            }
            TypeExpr::App {
                ctor: TypeCtor::Nominal(n),
                params,
            } if nominals.contains_key(&n) => {
                sink(n, via.map(str::to_string));
                let Some(held) = stored.get(&n) else { return };
                let rendered;
                let via = match via {
                    Some(v) => v,
                    None => {
                        rendered = ws.render(None, ty);
                        &rendered
                    }
                };
                for (i, idx) in params.into_iter().enumerate() {
                    if held.contains(&i) {
                        walk(
                            ws,
                            nominals,
                            stored,
                            TyId::Static(mid, idx),
                            Some(via),
                            sink,
                        );
                    }
                }
            }
            _ => {}
        }
    }
    walk(ws, nominals, stored, ty, None, sink)
}

/// The shortest edge path from `from` back to `goal`, as (nominal, via) steps
/// ending at `goal`; `from == goal` is the empty-tail path.
fn path_back(
    edges: &FxHashMap<Ustr, Vec<Edge>>,
    from: Ustr,
    goal: Ustr,
) -> Option<Vec<(Ustr, Option<String>)>> {
    // Each step records the nominal entered and the edge that entered it.
    let mut prev: FxHashMap<Ustr, Option<(Ustr, usize)>> = FxHashMap::default();
    prev.insert(from, None);
    let mut queue = std::collections::VecDeque::from([from]);
    while let Some(at) = queue.pop_front() {
        if at == goal {
            let mut steps = Vec::new();
            let mut cur = at;
            while let Some(Some((parent, edge_ix))) = prev.get(&cur).copied() {
                steps.push((cur, edges[&parent][edge_ix].via.clone()));
                cur = parent;
            }
            steps.push((from, None));
            steps.reverse();
            return Some(steps);
        }
        for (ix, edge) in edges.get(&at).into_iter().flatten().enumerate() {
            if let std::collections::hash_map::Entry::Vacant(slot) = prev.entry(edge.target) {
                slot.insert(Some((at, ix)));
                queue.push_back(edge.target);
            }
        }
    }
    None
}
