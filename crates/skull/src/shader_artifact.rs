//! The GLSL half of an artifact (`docs/monomorphization-plan.md` S 9, S 11,
//! S 20a).
//!
//! A [`TargetSpec`] carries an OPTIONAL shader axis beside its CPU axes, and
//! this is what that axis packages: the concrete shader entries the artifact
//! registers, the helpers those entries actually reach, and GLSL's own
//! rejections over exactly that retained set.
//!
//! **It realizes the SAME shared graph the CPU view does.** [`realize_shader`]
//! takes `&InstanceGraph` and walks the recorded call sites, so there is no
//! second reachability mechanism and no shader-specific graph -- the GPU side
//! of one program is a different ROOT SET over one instance graph, which is
//! the whole content of "one target-neutral graph, several artifact views".
//!
//! **The profile is the axis's typed value.** `skull::mir_emit` picks
//! `Core330` because the runtime GL context performs the GLES adaptation; that
//! is a packaging decision, and a view that re-derived it could not report
//! what a target actually carries.
//!
//! **GLSL's rejections stay GLSL's.** Recursion (no call stack) and the strict
//! tier-0 signature pass are `skull_codegen_glsl`'s, called here over the
//! retained set rather than reimplemented -- a second copy would be free to
//! disagree with the emitter. And they are the SHADER artifact's findings: the
//! CPU artifact over the same program compiles a recursive fn happily, which
//! is why shader decisions may not reach CPU admission.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bone_semantic::inference::Workspace;
use skull_codegen_glsl::GlslProfile;
use skull_mir::mir::FuncRef;
use skull_parser::Span;

use crate::instance_graph::{DeclKey, Instance, InstanceGraph, SiteOutcome};
use crate::target_projection::{ShaderTargetSpec, TargetSpec};

/// Everything one shader artifact view is taken through.
pub struct ShaderArtifactRequest<'a> {
    pub spec: TargetSpec,
    /// The shader ENTRIES, an input exactly like the CPU artifact's emission
    /// roots. Production reads them from `skull_mir::lower`'s `rock_shader`
    /// collector; the view never re-derives them.
    pub roots: &'a [FuncRef],
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ShaderRejectionKind {
    /// A cycle among retained shader instances. GLSL has no call stack, so this
    /// is a rejection and not a cost.
    Recursion,
    /// The strict tier-0 pass refused this declaration's signature.
    Profile(String),
}

/// One retained shader instance this artifact cannot package, with the span the
/// diagnostic attaches to at the Stage-3 cutover.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShaderRejection {
    pub instance: Instance,
    pub kind: ShaderRejectionKind,
    pub span: Span,
}

/// One target's SHADER artifact.
pub struct ShaderArtifactView {
    pub profile: GlslProfile,
    /// The entries this artifact registers, as instances of the shared graph.
    pub roots: BTreeSet<Instance>,
    /// Helpers reached from those entries. Disjoint from `roots`.
    pub helpers: BTreeSet<Instance>,
    /// Instances rendered INLINE at call sites (a GLSL intrinsic template or a
    /// Vec operator's native infix). Reached, never traversed and never
    /// emitted as a definition -- `skull_codegen_glsl`'s own leaf rule.
    pub builtins: BTreeSet<Instance>,
    pub rejections: Vec<ShaderRejection>,
    /// Call sites the retention walk traversed -- the denominator for
    /// `rejections`, so zero rejections over zero sites cannot read as clean.
    pub sites_walked: usize,
}

impl ShaderArtifactView {
    /// Every packaged instance: entries plus helpers. Builtins are excluded --
    /// they have no GLSL definition to package.
    pub fn packaged(&self) -> impl Iterator<Item = &Instance> {
        self.roots.iter().chain(self.helpers.iter())
    }

    pub fn render_rejections(&self, ws: &Workspace) -> Vec<String> {
        self.rejections
            .iter()
            .map(|r| {
                let f = &ws.module(r.instance.func.module).functions[r.instance.func.func];
                match &r.kind {
                    ShaderRejectionKind::Recursion => {
                        format!("`{}`: recursion is not allowed on GLSL", f.name)
                    }
                    ShaderRejectionKind::Profile(m) => format!("`{}`: {m}", f.name),
                }
            })
            .collect()
    }
}

/// Realize the shader half of `req.spec` over the shared `g`, or `None` when
/// this target packages no GLSL beside its CPU artifact.
///
/// `g` is `&`, so this cannot discover any more than the CPU view can.
pub fn realize_shader(
    ws: &Workspace,
    g: &InstanceGraph,
    req: &ShaderArtifactRequest<'_>,
) -> Option<ShaderArtifactView> {
    // The typed axis value. A non-GLSL shader target packages no GLSL artifact
    // -- and says so by being absent here rather than by defaulting to a
    // profile it does not have.
    let profile = match req.spec.shader? {
        ShaderTargetSpec::Glsl(p) => p,
        ShaderTargetSpec::SpirV | ShaderTargetSpec::Wgsl | ShaderTargetSpec::Msl => return None,
    };

    let root_decls: BTreeSet<DeclKey> = req
        .roots
        .iter()
        .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, *r)))
        .collect();
    let roots: BTreeSet<Instance> =
        g.instances.iter().filter(|i| root_decls.contains(&DeclKey(i.func))).cloned().collect();

    // Retention: the same walk shape the CPU view runs, over the same recorded
    // call sites, from a DIFFERENT root set. A callee that renders inline is a
    // leaf, which is `skull_codegen_glsl`'s own reachability rule (`Reach`).
    let mut helpers: BTreeSet<Instance> = BTreeSet::new();
    let mut builtins: BTreeSet<Instance> = BTreeSet::new();
    let mut seen: BTreeSet<Instance> = roots.clone();
    let mut queue: VecDeque<Instance> = roots.iter().cloned().collect();
    let mut edges: BTreeMap<Instance, Vec<(Instance, Span)>> = BTreeMap::new();
    let mut sites_walked = 0usize;
    while let Some(caller) = queue.pop_front() {
        let Some(sites) = g.call_sites.get(&caller) else { continue };
        for rec in sites.values() {
            sites_walked += 1;
            let SiteOutcome::Resolved(callee) = &rec.outcome else { continue };
            let f = &ws.module(callee.func.module).functions[callee.func.func];
            if skull_codegen_glsl::renders_inline(f) {
                builtins.insert(callee.clone());
                continue;
            }
            edges.entry(caller.clone()).or_default().push((callee.clone(), rec.span));
            if seen.insert(callee.clone()) {
                helpers.insert(callee.clone());
                queue.push_back(callee.clone());
            }
        }
    }

    let mut rejections: Vec<ShaderRejection> = Vec::new();
    rejections.extend(recursion_rejections(&roots, &edges));
    // The strict tier-0 pass, over the HELPERS this artifact retains -- GLSL's
    // own function, so the view cannot admit a signature the emitter refuses.
    //
    // Not over the ENTRIES: an entry's signature is classified by stage
    // (`classify_entry` / `classify_vertex`), where an `@uniform` struct param
    // is legal and a tier-0 leaf is not the question. Asking the library check
    // there reported 1,539 corpus rejections, every one a real shader this
    // repo ships (`bloom_blur_frag`'s `BloomBlurU`).
    for i in helpers.iter() {
        let f = &ws.module(i.func.module).functions[i.func.func];
        if let Err(e) = skull_codegen_glsl::check_tier0_signature(ws, f) {
            rejections.push(ShaderRejection {
                instance: i.clone(),
                kind: ShaderRejectionKind::Profile(e.message),
                span: f.name_span,
            });
        }
    }

    Some(ShaderArtifactView { profile, roots, helpers, builtins, rejections, sites_walked })
}

/// Cycles in the RETAINED shader call graph, one rejection per instance that
/// reaches itself.
///
/// Over the recorded edges, not a second resolution of the program: the walk
/// above already asked what each site calls, and this only asks whether that
/// relation has a cycle -- which is the property GLSL cannot compile.
fn recursion_rejections(
    roots: &BTreeSet<Instance>,
    edges: &BTreeMap<Instance, Vec<(Instance, Span)>>,
) -> Vec<ShaderRejection> {
    let mut out: Vec<ShaderRejection> = Vec::new();
    let mut done: BTreeSet<Instance> = BTreeSet::new();
    let mut reported: BTreeSet<Instance> = BTreeSet::new();
    // Iterative DFS with an explicit path, so the SPAN reported is the call
    // edge that closed the cycle rather than a declaration's own name.
    for root in roots {
        let mut path: Vec<(Instance, usize)> = vec![(root.clone(), 0)];
        let mut on_path: BTreeSet<Instance> = [root.clone()].into_iter().collect();
        while let Some((node, idx)) = path.pop() {
            let next = edges.get(&node).and_then(|es| es.get(idx)).cloned();
            match next {
                Some((callee, span)) => {
                    path.push((node, idx + 1));
                    if on_path.contains(&callee) {
                        if reported.insert(callee.clone()) {
                            out.push(ShaderRejection {
                                instance: callee,
                                kind: ShaderRejectionKind::Recursion,
                                span,
                            });
                        }
                        continue;
                    }
                    if done.contains(&callee) {
                        continue;
                    }
                    on_path.insert(callee.clone());
                    path.push((callee, 0));
                }
                None => {
                    on_path.remove(&node);
                    done.insert(node);
                }
            }
        }
    }
    out
}
