//! Type-directed completion over field and UFCS return edges. The index is
//! reused across body edits. Type reachability alone cannot bound the number
//! of access paths: chained diamonds have exponentially many simple paths.
//! Query search therefore keeps ranked partition minima rather than all paths.

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
    Method {
        takes_args: bool,
    },
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
    #[cfg(test)]
    search_visits: std::sync::atomic::AtomicUsize,
    edges: HashMap<Ustr, Vec<Edge>>,
    /// `reaches[T]` = every type reachable from `T` with its shortest distance.
    /// Prunes searches; it does not store access paths.
    reaches: HashMap<Ustr, HashMap<Ustr, u16>>,
    /// Shallow call-through argument paths, cached independently of the
    /// expected-type search because their depth is bounded at build time.
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
                        field: Some(FieldRef {
                            owner: src,
                            vis: f.read_vis.clone(),
                        }),
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
                let Some(p0) = f.param_decls.first() else {
                    continue;
                };
                let Some(recv) = struct_name_of_ty(ws, p0.ty) else {
                    continue;
                };
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
                let Some(outs) = edges.get(&cur) else {
                    continue;
                };
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
            enumerate_arg_paths(
                &edges,
                src,
                "",
                0,
                false,
                &mut on_path,
                &mut paths,
                &[],
                &[],
            );
            if !paths.is_empty() {
                arg_paths.insert(src, paths);
            }
        }

        ReachabilityIndex {
            #[cfg(test)]
            search_visits: Default::default(),
            edges,
            reaches,
            arg_paths,
        }
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
        format!(
            "{} nodes, {edges} edges, {pairs} closure-pairs",
            self.edges.len()
        )
    }

    fn reaches_any(&self, from: Ustr, expected: &[Ustr]) -> bool {
        self.reaches
            .get(&from)
            .is_some_and(|d| expected.iter().any(|e| d.contains_key(e)))
    }

    /// Simple paths ranked by (distance, suffix), retaining at most `cap`.
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
        #[cfg(test)]
        self.search_visits
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let root = Ustr::from(root_ty.strip_suffix('?').unwrap_or(root_ty));
        // Nullable links share nominal keys; callers supply any null check.
        let want: Vec<Ustr> = expected
            .iter()
            .map(|s| Ustr::from(s.strip_suffix('?').unwrap_or(s)))
            .collect();
        let mut result = ReachResult {
            paths: Vec::new(),
            truncated: false,
        };
        if !self.reaches_any(root, &want) {
            return result;
        }
        let mut pending = std::collections::BTreeMap::new();
        let mut serial = 0usize;
        let prefix = Vec::new();
        let blocked = HashSet::new();
        if let Some(path) = self.shortest_path(root, &want, &prefix, &blocked, visibility) {
            pending.insert(
                (path.len(), self.path_suffix(&path, false), serial),
                (path, prefix, blocked),
            );
        }
        while let Some((_, (path, prefix, blocked))) = pending.pop_first() {
            if result.paths.len() == cap {
                result.truncated = true;
                break;
            }
            result.paths.push(self.materialize_path(&path));
            // Partition alternatives by their first different edge (or stop).
            // Each partition contributes only its minimum, so we never enumerate
            // every path before applying the requested rank/cap.
            for split in prefix.len()..=path.len() {
                let alternative_prefix = path[..split].to_vec();
                let mut alternative_blocked = if split == prefix.len() {
                    blocked.clone()
                } else {
                    HashSet::new()
                };
                alternative_blocked.insert(path.get(split).map(|(_, edge)| *edge));
                if let Some(alternative) = self.shortest_path(
                    root,
                    &want,
                    &alternative_prefix,
                    &alternative_blocked,
                    visibility,
                ) {
                    serial += 1;
                    pending.insert(
                        (
                            alternative.len(),
                            self.path_suffix(&alternative, false),
                            serial,
                        ),
                        (alternative, alternative_prefix, alternative_blocked),
                    );
                    // A partition whose minimum is already outside the remaining
                    // result slots cannot contribute an earlier result later.
                    let remaining = cap.saturating_sub(result.paths.len()).saturating_add(1);
                    if pending.len() > remaining {
                        pending.pop_last();
                    }
                }
            }
        }
        result
    }

    fn path_suffix(&self, path: &[(Ustr, usize)], continuing: bool) -> String {
        path.iter()
            .enumerate()
            .map(|(i, (node, edge))| {
                let edge = &self.edges[node][*edge];
                if matches!(edge.kind, EdgeKind::Method { .. })
                    && (continuing || i + 1 < path.len())
                {
                    format!("{}()", edge.label)
                } else {
                    edge.label.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(".")
    }

    fn materialize_path(&self, path: &[(Ustr, usize)]) -> ReachPath {
        let edges: Vec<_> = path
            .iter()
            .map(|(node, edge)| &self.edges[node][*edge])
            .collect();
        let last = edges.last().expect("reach paths have an edge");
        ReachPath {
            suffix: self.path_suffix(path, false),
            final_ty: last.target.to_string(),
            is_method: matches!(last.kind, EdgeKind::Method { .. }),
            takes_args: matches!(last.kind, EdgeKind::Method { takes_args: true }),
            via_call: edges
                .iter()
                .any(|e| matches!(e.kind, EdgeKind::Method { .. })),
            dist: path.len() as u16,
            method_members: edges.iter().filter_map(|e| e.method).collect(),
            field_members: edges.iter().filter_map(|e| e.field.clone()).collect(),
        }
    }

    fn shortest_path(
        &self,
        root: Ustr,
        want: &[Ustr],
        prefix: &[(Ustr, usize)],
        blocked: &HashSet<Option<usize>>,
        visibility: Option<(&Workspace, &FragmentScope)>,
    ) -> Option<Vec<(Ustr, usize)>> {
        let last = prefix.last().map(|(node, edge)| &self.edges[node][*edge]);
        let start = last.map_or(root, |edge| edge.target);
        if !prefix.is_empty() && want.contains(&start) && !blocked.contains(&None) {
            return Some(prefix.to_vec());
        }
        if last.is_some_and(|edge| matches!(edge.kind, EdgeKind::Method { takes_args: true })) {
            return None;
        }
        let prefix_nodes: HashSet<_> = prefix.iter().map(|(node, _)| *node).collect();
        // Include the separator: `a().x` sorts before `a.x`, although `a`
        // sorts before `a()` when compared without their continuations.
        let key = (prefix.len(), format!("{}.", self.path_suffix(prefix, true)));
        let mut best = HashMap::from([(start, key.clone())]);
        let mut pending =
            std::collections::BTreeMap::from([((key.0, key.1, 0usize), (start, prefix.to_vec()))]);
        let mut serial = 0usize;
        let mut goal: Option<(usize, String, Vec<(Ustr, usize)>)> = None;
        while let Some(((distance, spelling, _), (node, path))) = pending.pop_first() {
            if best.get(&node) != Some(&(distance, spelling)) {
                continue;
            }
            if goal.as_ref().is_some_and(|(len, _, _)| distance + 1 > *len) {
                break;
            }
            #[cfg(test)]
            self.search_visits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(edges) = self.edges.get(&node) else {
                continue;
            };
            for (index, edge) in edges.iter().enumerate() {
                if (node == start && blocked.contains(&Some(index)))
                    || prefix_nodes.contains(&edge.target)
                    || edge.target == node
                    || path.iter().any(|(from, _)| *from == edge.target)
                {
                    continue;
                }
                if let Some((ws, frag)) = visibility {
                    if edge.method.as_ref().is_some_and(|method| {
                        !methods_admitted(ws, frag, std::slice::from_ref(method))
                    }) || edge.field.as_ref().is_some_and(|field| {
                        !fields_admitted(ws, frag, std::slice::from_ref(field))
                    }) {
                        continue;
                    }
                }
                let mut next = path.clone();
                next.push((node, index));
                if want.contains(&edge.target) {
                    let suffix = self.path_suffix(&next, false);
                    if goal
                        .as_ref()
                        .is_none_or(|(len, old, _)| (next.len(), &suffix) < (*len, old))
                    {
                        goal = Some((next.len(), suffix, next.clone()));
                    }
                }
                if matches!(edge.kind, EdgeKind::Method { takes_args: true })
                    || !self.reaches_any(edge.target, want)
                {
                    continue;
                }
                let key = (next.len(), format!("{}.", self.path_suffix(&next, true)));
                if best.get(&edge.target).is_none_or(|old| &key < old) {
                    best.insert(edge.target, key.clone());
                    serial += 1;
                    pending.insert((key.0, key.1, serial), (edge.target, next));
                }
            }
        }
        goal.map(|(_, _, path)| path)
    }
}

/// Build-time enumeration of every depth-`<= ARG_PATH_DEPTH` access path from
/// `cur` to any type (simple-path cycle-guarded). Uses the query suffix shaping
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
        let leaf_suffix = if prefix.is_empty() {
            e.label.clone()
        } else {
            format!("{prefix}.{}", e.label)
        };
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
        let traversable = matches!(
            e.kind,
            EdgeKind::Field | EdgeKind::Method { takes_args: false }
        );
        if traversable && depth + 1 < ARG_PATH_DEPTH {
            let seg_label = match e.kind {
                EdgeKind::Field => e.label.clone(),
                EdgeKind::Method { .. } => format!("{}()", e.label),
            };
            let descend_prefix = if prefix.is_empty() {
                seg_label
            } else {
                format!("{prefix}.{seg_label}")
            };
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

pub(crate) fn methods_admitted(
    ws: &Workspace,
    frag: &FragmentScope,
    methods: &[MethodRef],
) -> bool {
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
    let TyId::Static(m, idx) = ty else {
        return None;
    };
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

#[cfg(test)]
mod search_tests {
    use super::*;

    #[test]
    fn requested_cap_bounds_work_on_layered_diamonds() {
        let mut source = "struct Goal { value: int }\n".to_string();
        for level in 0..10 {
            let next = if level == 9 {
                "Goal".to_string()
            } else {
                format!("N{}", level + 1)
            };
            source.push_str(&format!("struct N{level} {{ a: {next}, b: {next} }}\n"));
        }
        let module = skull_parser::parse_module_clean(&source, Ustr::from("<reach-test>")).unwrap();
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut [module]).unwrap();
        let index = ReachabilityIndex::build(&ws);
        let result = index.paths_to("N0", &["Goal".to_string()], 1);
        assert_eq!(
            result
                .paths
                .iter()
                .map(|p| p.suffix.as_str())
                .collect::<Vec<_>>(),
            ["a.a.a.a.a.a.a.a.a.a"]
        );
        assert!(result.truncated);
        let visits = index
            .search_visits
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            visits <= 144,
            "one result must not enumerate all 1024 paths: {visits} visits"
        );
    }
}
