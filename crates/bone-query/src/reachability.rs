//! Type-directed completion over field and UFCS return edges. Chained
//! diamonds have exponentially many simple paths, so queries keep ranked
//! partition minima rather than all paths.

use bone_core::modules::{FragmentScope, NsVisibility};
use bone_core::prelude::Nominal;
use bone_semantic::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EdgeKind {
    Field,
    /// `takes_args`: a parameter remains after the receiver. Such a method can
    /// only be the last hop of a path, never traversed through.
    Method {
        takes_args: bool,
    },
}

struct Edge {
    label: String,
    kind: EdgeKind,
    /// Rendered type this edge leads to (a node key).
    target: Ustr,
    method: Option<MethodRef>,
    field: Option<FieldRef>,
}

pub(crate) type MethodRef = (Ustr, ModuleId, Idx<bone_semantic::hir::Function>);

#[derive(Clone)]
pub(crate) struct FieldRef {
    owner: Ustr,
    vis: NsVisibility,
}

/// `suffix` is relative to the root, with `()` on every intermediate method
/// hop but not the final one; the caller renders the final hop from the flags.
pub struct ReachPath {
    pub suffix: String,
    pub final_ty: String,
    pub is_method: bool,
    /// Only meaningful when `is_method`: the final method needs a snippet hole.
    pub takes_args: bool,
    /// Any hop is a method call; `is_method` describes only the final hop.
    pub via_call: bool,
    pub dist: u16,
    pub(crate) method_members: Vec<MethodRef>,
    pub(crate) field_members: Vec<FieldRef>,
}

pub struct ReachResult {
    pub paths: Vec<ReachPath>,
    /// The result cap clipped some paths.
    pub truncated: bool,
}

/// Derived data is computed on first use: the index is rebuilt on every
/// signature edit, while queries touch only a few types.
pub struct ReachabilityIndex {
    #[cfg(test)]
    search_visits: std::sync::atomic::AtomicUsize,
    edges: HashMap<Ustr, Vec<Edge>>,
    preds: FxHashMap<Ustr, Vec<Ustr>>,
    /// Per sorted expected-type set: every type with a path to one of them.
    /// Prunes searches only.
    reachers: Mutex<FxHashMap<Vec<Ustr>, Arc<FxHashSet<Ustr>>>>,
    /// Shallow call-through argument paths, filled on first lookup.
    arg_paths: FxHashMap<Ustr, OnceLock<Vec<ReachPath>>>,
}

/// Call-through args are `root.field.field` shaped; deeper paths would
/// explode the store.
const ARG_PATH_DEPTH: u16 = 2;

impl ReachabilityIndex {
    /// Lays edges and predecessor lists; derived data is filled per query.
    pub fn build(ws: &Workspace) -> ReachabilityIndex {
        let mut edges: HashMap<Ustr, Vec<Edge>> = HashMap::new();

        // Keyed by the struct's bare name, which is how a value of that struct
        // type renders.
        for m in ws.modules.iter() {
            for (&src, &def) in m.nominals.iter() {
                let Nominal::Struct(sid) = def else { continue };
                let s = &m.structs[sid.def];
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

        for m in ws.modules.iter() {
            for (fid, f) in m.functions.iter() {
                if bone_core::modules::display_name(f.name.as_str()).starts_with("__") {
                    continue;
                }
                let Some(p0) = f.param_decls.first() else {
                    continue;
                };
                // Arrays and other builtin constructors use separate completion paths
                // and would flood the reach graph.
                if crate::types::nominal_name(ws, None, p0.ty).is_none() {
                    continue;
                }
                let recv = type_key(ws, p0.ty);
                let Some(ret) = f.return_ty else { continue };
                let rendered = ws.render(None, ret);
                if rendered == "()" || rendered.is_empty() {
                    continue;
                }
                edges.entry(recv).or_default().push(Edge {
                    label: bone_core::modules::display_name(f.name.as_str()).to_string(),
                    kind: EdgeKind::Method {
                        takes_args: f.param_decls.len() > 1,
                    },
                    target: type_key(ws, ret),
                    method: Some((f.name, m.id, fid)),
                    field: None,
                });
            }
        }

        let mut preds: FxHashMap<Ustr, Vec<Ustr>> = FxHashMap::default();
        for (&src, outs) in edges.iter() {
            for e in outs {
                preds.entry(e.target).or_default().push(src);
            }
        }
        let arg_paths = edges.keys().map(|&src| (src, OnceLock::new())).collect();

        ReachabilityIndex {
            #[cfg(test)]
            search_visits: Default::default(),
            edges,
            preds,
            reachers: Mutex::default(),
            arg_paths,
        }
    }

    /// Peels a nullable `?` so a `Foo?` root matches the `Foo`-keyed store.
    pub(crate) fn arg_paths_from(&self, ty: &str) -> &[ReachPath] {
        let key = Ustr::from(ty.strip_suffix('?').unwrap_or(ty));
        self.arg_paths.get(&key).map_or(&[], |slot| {
            slot.get_or_init(|| {
                let mut paths = Vec::new();
                let mut on_path: HashSet<Ustr> = HashSet::new();
                on_path.insert(key);
                enumerate_arg_paths(
                    &self.edges,
                    key,
                    "",
                    0,
                    false,
                    &mut on_path,
                    &mut paths,
                    &[],
                    &[],
                );
                paths
            })
        })
    }

    /// Visibility is a property of the request site, so it is applied at read
    /// time to the workspace-wide cache.
    pub(crate) fn arg_paths_in_scope(
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

    fn reachers_of(&self, want: &[Ustr]) -> Arc<FxHashSet<Ustr>> {
        let mut key = want.to_vec();
        key.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        key.dedup();
        let mut memo = self.reachers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(set) = memo.get(&key) {
            return set.clone();
        }
        let mut set: FxHashSet<Ustr> = FxHashSet::default();
        let mut q: VecDeque<Ustr> = key.iter().copied().collect();
        while let Some(cur) = q.pop_front() {
            for &p in self.preds.get(&cur).map_or(&[][..], |v| v.as_slice()) {
                if set.insert(p) {
                    q.push_back(p);
                }
            }
        }
        let set = Arc::new(set);
        memo.insert(key, set.clone());
        set
    }

    /// Simple paths ranked by (distance, suffix), retaining at most `cap`.
    /// `visibility` applies per-member visibility for a modules-v1 query.
    pub fn paths_to(
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
        let reachers = self.reachers_of(&want);
        if !reachers.contains(&root) {
            return result;
        }
        let mut pending = std::collections::BTreeMap::new();
        let mut serial = 0usize;
        let prefix = Vec::new();
        let blocked = HashSet::new();
        if let Some(path) =
            self.shortest_path(root, &want, &reachers, &prefix, &blocked, visibility)
        {
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
            // Each partition by first different edge contributes only its minimum,
            // so we never enumerate every path before applying the rank and cap.
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
                    &reachers,
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
        reachers: &FxHashSet<Ustr>,
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
                    || !reachers.contains(&edge.target)
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

/// Unguided (records every reachable type) and depth-capped: deeper paths
/// would explode the store.
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
        // An args-taking method ends the path.
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

/// Bare nominal name (peeling `?`), else the rendered form, so a `next:
/// Node?` field edge reaches the `Node` key.
pub(crate) fn type_key(ws: &Workspace, ty: TyId) -> Ustr {
    type_key_in_body(ws, None, ty)
}

pub(crate) fn type_key_in_body(ws: &Workspace, bi: Option<&BodyInference>, ty: TyId) -> Ustr {
    crate::types::nominal_name(ws, bi, ty).unwrap_or_else(|| Ustr::from(ws.render(bi, ty).as_str()))
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
        let mut module =
            skull_parser::parse_module_clean(&source, Ustr::from("<reach-test>")).unwrap();
        module.package = Some(skull_parser::PackageTag {
            id: 1,
            self_name: "reach_test".to_string(),
            identity_storage: Default::default(),
            root: String::new(),
        });
        let ws = bone_semantic::semantic_passes::semantic_pass(&mut [module]).unwrap();
        let index = ReachabilityIndex::build(&ws);
        let result = index.paths_to("reach_test::N0", &["reach_test::Goal".to_string()], 1, None);
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
