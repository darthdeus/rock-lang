//! Type-reachability index for type-directed completion.
//!
//! Inside `f(|)` with a known expected param type `E`, we want every in-scope
//! path that PRODUCES an `E` -- through struct fields, UFCS method returns, and
//! mixtures (`player.movable.to_ivec()`). The graph (nodes = types, edges =
//! field accesses and method returns) is a pure function of the type graph, so
//! it is built once per Interface-tier change and reused across body keystrokes.
//!
//! Reachability of *types* is a transitive closure (the `reaches` DP table).
//! But completion inserts a *path string*, and path enumeration is a different
//! complexity class (a diamond `T->A->E`, `T->B->E` has 2 paths; k chained
//! diamonds have 2^k) -- memoizing the reachable SET does not collapse that. So
//! `reaches` is used as a PRUNING ORACLE: the query is a guided DFS that
//! descends an edge only when its target still reaches `E`, with a simple-path
//! cycle guard (no depth cap) and a UX result cap. Heuristic backlog:
//! completion pipeline contract.

use bone_core::modules::{FragmentScope, NsVisibility};
use bone_semantic::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EdgeKind {
    Field,
    /// A UFCS method. `takes_args` is true iff a parameter remains after the
    /// receiver fills the first one -- such a method can only be the LAST hop of
    /// a path (you cannot park a snippet hole mid-path), so it is never
    /// traversed through.
    Method { takes_args: bool },
}

struct Edge {
    label: String,
    kind: EdgeKind,
    /// Rendered type this edge leads to (a node key).
    target: Ustr,
    /// The callable family member behind a method edge. Fields have no member
    /// identity and therefore carry `None`.
    method: Option<MethodRef>,
    field: Option<FieldRef>,
}

pub(crate) type MethodRef = (Ustr, ModuleId, Idx<bone_semantic::hir::Function>);

#[derive(Clone)]
pub(crate) struct FieldRef {
    owner: Ustr,
    vis: NsVisibility,
}

/// One completed path from a root variable to a value of the expected type.
/// `suffix` is the dotted access RELATIVE to the root variable, with `()` baked
/// onto every INTERMEDIATE method hop but NOT the final one -- the caller
/// renders the final hop (`field` / `method()` / `method($0)`) from the flags.
pub struct ReachPath {
    pub suffix: String,
    pub final_ty: String,
    pub is_method: bool,
    /// Only meaningful when `is_method`: the final method needs a snippet hole.
    pub takes_args: bool,
    /// ANY hop on the path (intermediate or final) is a method call -- the
    /// "method-chain" classification call-through's per-fn cap keys off
    /// (`is_method` only describes the final hop; `x.foo().bar` traverses a
    /// call but ends on a field).
    pub via_call: bool,
    pub dist: u16,
    pub(crate) method_members: Vec<MethodRef>,
    pub(crate) field_members: Vec<FieldRef>,
}

pub struct ReachResult {
    pub paths: Vec<ReachPath>,
    /// True if the result cap clipped some paths (the caller logs this -- no
    /// silent truncation).
    pub truncated: bool,
}

/// Edges out of each type, plus the transitive closure used to prune the
/// per-keystroke path search.
pub struct ReachabilityIndex {
    edges: HashMap<Ustr, Vec<Edge>>,
    /// `reaches[T]` = every type reachable from `T` with its shortest distance.
    /// The pruning oracle for the on-demand deep-path DFS (`paths_to`), NOT the
    /// path store.
    reaches: HashMap<Ustr, HashMap<Ustr, u16>>,
    /// `arg_paths[S]` = every depth-`<= ARG_PATH_DEPTH` access path from a value
    /// of type `S` to any type, materialized at build time. Shallow paths don't
    /// explode (bounded by out-degree^depth), so unlike deep paths they CAN be
    /// stored and queried as a lookup -- which is what the per-keystroke
    /// call-through arg expansion does (`arg_paths_from`), instead of the
    /// per-call guided DFS `paths_to` runs. Built once per inference change.
    arg_paths: HashMap<Ustr, Vec<ReachPath>>,
}

/// Max depth for the precomputed `arg_paths` store. Call-through args are
/// `root.field.field` / `root.method()` shaped (`player.movable.grid`), so depth
/// 2 covers them; deeper paths would explode the store (the reason `paths_to`
/// stays on-demand for the rare, deep member-completion case).
const ARG_PATH_DEPTH: u16 = 2;

impl ReachabilityIndex {
    /// Build from a checked workspace. O(structs*fields + functions) to lay
    /// edges, then a BFS per source for the closure (sub-ms at game-scale node
    /// counts; the SCC-condensation DP is the documented asymptotic follow-up).
    pub fn build(ws: &Workspace) -> ReachabilityIndex {
        let mut edges: HashMap<Ustr, Vec<Edge>> = HashMap::new();

        // Field edges: `S -.field-> render(field.ty)` for every struct field.
        // Keyed by the struct's bare name, which is exactly how a value of that
        // struct type renders (so it matches a root variable's rendered type).
        for m in ws.modules.iter() {
            for s in m.structs.values() {
                let src = s.name;
                for f in s.fields.iter() {
                    edges.entry(src).or_default().push(Edge {
                        label: f.name.to_string(),
                        kind: EdgeKind::Field,
                        target: type_key(ws, f.ty),
                        method: None,
                        field: Some(FieldRef { owner: src, vis: f.read_vis.clone() }),
                    });
                }
            }
        }

        // Method edges: `recv -.m()-> render(return)` for every UFCS function
        // (first param a struct) returning a usable (non-unit) value. ONE pass
        // over all functions -- this replaces the per-root-per-keystroke
        // `ufcs_methods` scan.
        for m in ws.modules.iter() {
            for (fid, f) in m.functions.iter() {
                if f.name.as_str().starts_with("__") {
                    continue;
                }
                let Some(p0) = f.param_decls.first() else { continue };
                let Some(recv) = struct_name_of_ty(ws, p0.ty) else { continue };
                let Some(ret) = f.return_ty else { continue };
                let rendered = ws.render(None, ret);
                if rendered == "()" || rendered.is_empty() {
                    continue;
                }
                edges.entry(recv).or_default().push(Edge {
                    label: f.name.to_string(),
                    kind: EdgeKind::Method {
                        takes_args: f.param_decls.len() > 1,
                    },
                    target: type_key(ws, ret),
                    method: Some((f.name, m.id, fid)),
                    field: None,
                });
            }
        }

        // Transitive closure (the pruning oracle): BFS from every node, recording
        // the shortest distance to each reachable target. BFS naturally tolerates
        // cycles via the visited set.
        let mut reaches: HashMap<Ustr, HashMap<Ustr, u16>> = HashMap::new();
        for &src in edges.keys() {
            let mut dist: HashMap<Ustr, u16> = HashMap::new();
            let mut q: VecDeque<(Ustr, u16)> = VecDeque::new();
            q.push_back((src, 0));
            while let Some((cur, d)) = q.pop_front() {
                let Some(outs) = edges.get(&cur) else { continue };
                for e in outs {
                    let nd = d + 1;
                    // Keep the first (=shortest, BFS) distance seen per target.
                    if dist.get(&e.target).is_none_or(|&old| nd < old) {
                        if dist.insert(e.target, nd).is_none() {
                            q.push_back((e.target, nd));
                        }
                    }
                }
            }
            reaches.insert(src, dist);
        }

        // Materialize the shallow (depth <= ARG_PATH_DEPTH) path store: a simple
        // bounded enumeration per source. This is the work `paths_to` used to do
        // per call for the call-through arg expansion -- done once here instead.
        let mut arg_paths: HashMap<Ustr, Vec<ReachPath>> = HashMap::new();
        for &src in edges.keys() {
            let mut paths = Vec::new();
            let mut on_path: HashSet<Ustr> = HashSet::new();
            on_path.insert(src);
            enumerate_arg_paths(&edges, src, "", 0, false, &mut on_path, &mut paths, &[], &[]);
            if !paths.is_empty() {
                arg_paths.insert(src, paths);
            }
        }

        ReachabilityIndex { edges, reaches, arg_paths }
    }

    /// Precomputed depth-`<= ARG_PATH_DEPTH` reach paths from a value of `ty` to
    /// any type -- a lookup (no DFS). The call-through arg expansion filters
    /// these by the wanted param type. Empty for a scalar / leaf type. Peels a
    /// nullable `?` so a `Foo?` root matches the `Foo`-keyed store.
    pub fn arg_paths_from(&self, ty: &str) -> &[ReachPath] {
        let key = Ustr::from(ty.strip_suffix('?').unwrap_or(ty));
        self.arg_paths.get(&key).map_or(&[], |v| v.as_slice())
    }

    /// The same shallow paths, restricted to callable members admitted by a
    /// modules-v1 fragment. The cached graph is workspace-wide; visibility is
    /// a property of the request site, so it is applied at read time.
    pub fn arg_paths_in_scope(
        &self,
        ws: &Workspace,
        frag: &FragmentScope,
        ty: &str,
    ) -> Vec<&ReachPath> {
        self.arg_paths_from(ty)
            .iter()
            .filter(|path| path_admitted(ws, frag, path))
            .collect()
    }

    /// `N nodes, E edges, R closure-pairs` -- for the build profiling line.
    pub fn stats(&self) -> String {
        let edges: usize = self.edges.values().map(|v| v.len()).sum();
        let pairs: usize = self.reaches.values().map(|m| m.len()).sum();
        format!("{} nodes, {edges} edges, {pairs} closure-pairs", self.edges.len())
    }

    fn reaches_any(&self, from: Ustr, expected: &[Ustr]) -> bool {
        self.reaches
            .get(&from)
            .is_some_and(|d| expected.iter().any(|e| d.contains_key(e)))
    }

    /// Every path from a value of `root_ty` to a value of one of `expected`,
    /// guided-DFS pruned by the closure, simple-path cycle-guarded, ranked by
    /// (distance, suffix) and capped at `cap`.
    pub fn paths_to(&self, root_ty: &str, expected: &[String], cap: usize) -> ReachResult {
        self.paths_to_impl(root_ty, expected, cap, None)
    }

    /// `paths_to` with per-member visibility applied for a modules-v1 query.
    pub fn paths_to_in_scope(
        &self,
        ws: &Workspace,
        frag: &FragmentScope,
        root_ty: &str,
        expected: &[String],
        cap: usize,
    ) -> ReachResult {
        self.paths_to_impl(root_ty, expected, cap, Some((ws, frag)))
    }

    fn paths_to_impl(
        &self,
        root_ty: &str,
        expected: &[String],
        cap: usize,
        visibility: Option<(&Workspace, &FragmentScope)>,
    ) -> ReachResult {
        let root = Ustr::from(root_ty.strip_suffix('?').unwrap_or(root_ty));
        // Node keys are canonical nominal names (no nullable `?`), so match the
        // expected types the same way -- a nullable link can still satisfy a
        // non-null target (the user can null-check / `?.`).
        let want: Vec<Ustr> = expected
            .iter()
            .map(|s| Ustr::from(s.strip_suffix('?').unwrap_or(s)))
            .collect();
        let mut out: Vec<ReachPath> = Vec::new();
        // Bail unless this root can actually reach a wanted type -- avoids any
        // DFS for the many scalar / unrelated roots.
        if !self.edges.contains_key(&root) || !self.reaches_any(root, &want) {
            return ReachResult { paths: out, truncated: false };
        }
        let mut on_path: HashSet<Ustr> = HashSet::new();
        on_path.insert(root);
        self.dfs(
            root,
            String::new(),
            0,
            false,
            &want,
            &mut on_path,
            &mut out,
            &[],
            &[],
            visibility,
        );

        out.sort_by(|a, b| a.dist.cmp(&b.dist).then_with(|| a.suffix.cmp(&b.suffix)));
        let truncated = out.len() > cap;
        out.truncate(cap);
        ReachResult { paths: out, truncated }
    }

    fn dfs(
        &self,
        cur: Ustr,
        prefix: String,
        depth: u16,
        prefix_call: bool,
        want: &[Ustr],
        on_path: &mut HashSet<Ustr>,
        out: &mut Vec<ReachPath>,
        method_members: &[MethodRef],
        field_members: &[FieldRef],
        visibility: Option<(&Workspace, &FragmentScope)>,
    ) {
        let Some(outs) = self.edges.get(&cur) else { return };
        for e in outs {
            let mut next_methods = method_members.to_vec();
            if let Some(method) = e.method {
                next_methods.push(method);
            }
            let mut next_fields = field_members.to_vec();
            if let Some(field) = &e.field {
                next_fields.push(field.clone());
            }
            if let Some((ws, frag)) = visibility {
                if !methods_admitted(ws, frag, &next_methods)
                    || !fields_admitted(ws, frag, &next_fields)
                {
                    continue;
                }
            }
            // Simple-path cycle guard: never re-enter a type already on this
            // path. Finiteness comes from here, not a depth cap.
            if on_path.contains(&e.target) {
                continue;
            }
            let seg_label = match e.kind {
                EdgeKind::Field => e.label.clone(),
                EdgeKind::Method { .. } => format!("{}()", e.label),
            };
            let leaf_suffix =
                if prefix.is_empty() { e.label.clone() } else { format!("{prefix}.{}", e.label) };

            // Productive leaf: this edge's target IS a wanted type.
            if want.iter().any(|w| *w == e.target) {
                let (is_method, takes_args) = match e.kind {
                    EdgeKind::Field => (false, false),
                    EdgeKind::Method { takes_args } => (true, takes_args),
                };
                out.push(ReachPath {
                    suffix: leaf_suffix.clone(),
                    final_ty: e.target.to_string(),
                    is_method,
                    takes_args,
                    via_call: prefix_call || is_method,
                    dist: depth + 1,
                    method_members: next_methods.clone(),
                    field_members: next_fields.clone(),
                });
            }

            // Descend only through TRAVERSABLE edges (fields + no-arg methods)
            // whose target still reaches a wanted type -- the closure prunes
            // every dead branch, so only productive paths are walked.
            let traversable = matches!(e.kind, EdgeKind::Field | EdgeKind::Method { takes_args: false });
            if traversable && self.reaches_any(e.target, want) {
                let hop_call = matches!(e.kind, EdgeKind::Method { .. });
                let descend_prefix =
                    if prefix.is_empty() { seg_label } else { format!("{prefix}.{seg_label}") };
                on_path.insert(e.target);
                self.dfs(
                    e.target,
                    descend_prefix,
                    depth + 1,
                    prefix_call || hop_call,
                    want,
                    on_path,
                    out,
                    &next_methods,
                    &next_fields,
                    visibility,
                );
                on_path.remove(&e.target);
            }
        }
    }
}

/// Build-time enumeration of every depth-`<= ARG_PATH_DEPTH` access path from
/// `cur` to any type (simple-path cycle-guarded). Mirrors `dfs`'s suffix shaping
/// (`suffix` is the bare access path; `is_method`/`takes_args` let the caller
/// render the final `()`), but is UNGUIDED (no `want` target -- it records every
/// reachable type, since the query's wanted type isn't known at build time) and
/// depth-capped (deeper paths would explode the store).
fn enumerate_arg_paths(
    edges: &HashMap<Ustr, Vec<Edge>>,
    cur: Ustr,
    prefix: &str,
    depth: u16,
    prefix_call: bool,
    on_path: &mut HashSet<Ustr>,
    out: &mut Vec<ReachPath>,
    method_members: &[MethodRef],
    field_members: &[FieldRef],
) {
    let Some(outs) = edges.get(&cur) else { return };
    for e in outs {
        // Simple-path cycle guard: never re-enter a type already on this path.
        if on_path.contains(&e.target) {
            continue;
        }
        let mut next_methods = method_members.to_vec();
        if let Some(method) = e.method {
            next_methods.push(method);
        }
        let mut next_fields = field_members.to_vec();
        if let Some(field) = &e.field {
            next_fields.push(field.clone());
        }
        let leaf_suffix =
            if prefix.is_empty() { e.label.clone() } else { format!("{prefix}.{}", e.label) };
        let (is_method, takes_args) = match e.kind {
            EdgeKind::Field => (false, false),
            EdgeKind::Method { takes_args } => (true, takes_args),
        };
        out.push(ReachPath {
            suffix: leaf_suffix,
            final_ty: e.target.to_string(),
            is_method,
            takes_args,
            via_call: prefix_call || is_method,
            dist: depth + 1,
            method_members: next_methods.clone(),
            field_members: next_fields.clone(),
        });
        // Descend through traversable edges (fields + no-arg methods) until the
        // depth cap. An args-taking method ends the path (can't chain through it).
        let traversable = matches!(e.kind, EdgeKind::Field | EdgeKind::Method { takes_args: false });
        if traversable && depth + 1 < ARG_PATH_DEPTH {
            let seg_label = match e.kind {
                EdgeKind::Field => e.label.clone(),
                EdgeKind::Method { .. } => format!("{}()", e.label),
            };
            let descend_prefix =
                if prefix.is_empty() { seg_label } else { format!("{prefix}.{seg_label}") };
            on_path.insert(e.target);
            enumerate_arg_paths(
                edges,
                e.target,
                &descend_prefix,
                depth + 1,
                prefix_call || is_method,
                on_path,
                out,
                &next_methods,
                &next_fields,
            );
            on_path.remove(&e.target);
        }
    }
}

pub(crate) fn methods_admitted(ws: &Workspace, frag: &FragmentScope, methods: &[MethodRef]) -> bool {
    methods.iter().all(|(key, mid, fid)| {
        crate::modules_scope::callable_member_admitted(ws, frag, *key, *mid, *fid)
    })
}

pub(crate) fn path_admitted(ws: &Workspace, frag: &FragmentScope, path: &ReachPath) -> bool {
    methods_admitted(ws, frag, &path.method_members)
        && fields_admitted(ws, frag, &path.field_members)
}

pub(crate) fn fields_admitted(ws: &Workspace, frag: &FragmentScope, fields: &[FieldRef]) -> bool {
    fields.iter().all(|field| {
        let Some(meta) = ws.defs.get(&field.owner) else {
            return true;
        };
        ws.ns.can_access(
            &field.vis,
            meta.package,
            &meta.module,
            frag.package,
            &frag.module_path,
        )
    })
}

/// Canonical graph-node key for a type: its bare nominal name when it is a
/// nominal type (struct/enum, peeling any nullable `?`), else the rendered form
/// (scalars like `int`). Canonicalizing is what lets traversal follow a `next:
/// Node?` or `movable: Movable` field edge to the struct keyed by its bare name.
fn type_key(ws: &Workspace, ty: TyId) -> Ustr {
    struct_name_of_ty(ws, ty).unwrap_or_else(|| Ustr::from(ws.render(None, ty).as_str()))
}

/// The bare nominal name of a static type, peeling a nullable wrapper. Mirrors
/// `query::struct_name_of_ty` but for the static (signature) arena only -- field
/// and parameter types are always static, so no body is threaded.
fn struct_name_of_ty(ws: &Workspace, ty: TyId) -> Option<Ustr> {
    let TyId::Static(m, idx) = ty else { return None };
    let te = &ws.modules.get(m.index())?.ty_arena[idx];
    match te {
        bone_semantic::hir::TypeExpr::App {
            ctor: bone_semantic::hir::TypeCtor::Nominal(name),
            ..
        } => Some(*name),
        bone_semantic::hir::TypeExpr::Nullable(inner) => {
            struct_name_of_ty(ws, TyId::Static(m, *inner))
        }
        _ => None,
    }
}
