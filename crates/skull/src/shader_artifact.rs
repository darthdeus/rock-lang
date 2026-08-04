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

use crate::instance_graph::{Concreteness, DeclKey, Instance, InstanceGraph, SiteOutcome};
use crate::target_projection::{ShaderTargetSpec, TargetSpec};

/// One registered shader program: a `(vertex, fragment)` pair, or the
/// single-argument fullscreen form whose vertex stage the compiler supplies.
///
/// PAIRS, not a flat root list. A pipeline fragment's signature is classified
/// against its vertex stage's varyings type -- as a standalone entry the same
/// signature is refused outright -- so a view holding only the fragment could
/// not ask GLSL the question emission asks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShaderProgram {
    pub vert: Option<FuncRef>,
    pub frag: FuncRef,
}

/// Everything one shader artifact view is taken through.
pub struct ShaderArtifactRequest<'a> {
    pub spec: TargetSpec,
    /// The shader PROGRAMS, an input exactly like the CPU artifact's emission
    /// roots. Production reads them from `skull_mir::lower`'s `rock_shader`
    /// collector; the view never re-derives them.
    pub programs: &'a [ShaderProgram],
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ShaderRejectionKind {
    /// A cycle among retained shader instances. GLSL has no call stack, so this
    /// is a rejection and not a cost.
    Recursion,
    /// Anything else the strict pass refused: a signature outside tier 0, a
    /// body that cannot lower, a foreign body with no GLSL mapping, a
    /// wrong-stage builtin.
    Profile,
}

/// One retained shader instance this artifact cannot package, with the span the
/// diagnostic attaches to at the Stage-3 cutover.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShaderRejection {
    /// The ENTRY the GLSL pass refused. Its own signature, its body, or
    /// anything it reaches -- `message` and `span` name which.
    pub instance: Instance,
    pub kind: ShaderRejectionKind,
    /// The emitter's own diagnostic, verbatim.
    pub message: String,
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
    /// Retained instances the declaration-based GLSL passes cannot answer for:
    /// their substitution is not empty, so the declaration is not the shape the
    /// artifact packages. Reported, never silently validated -- and the PROGRAM
    /// that reaches one is not validated either (see `programs_unchecked`).
    /// Entries and reachable HELPERS both: `check_entry_profile` lowers the
    /// helpers too, so a concrete entry over a generic helper still gets a
    /// verdict about a shape nothing packages.
    pub unchecked_generic: BTreeSet<Instance>,
    /// **An invariant finding.** Retained shader instances the shared graph
    /// holds no substitution record for. Not "concrete by default": a missing
    /// record means a producer bypassed `reach`'s recorder, and a program that
    /// reaches one is excluded from validation exactly as a generic one is.
    pub missing_substitutions: BTreeSet<Instance>,
    /// Where the structural cycle finding and the GLSL pass disagree. Both
    /// answer "is this recursive"; keeping the comparison is what makes the
    /// typed `Recursion` kind evidence rather than a second opinion.
    pub disagreements: Vec<String>,
    /// PROGRAMS the GLSL pass actually ran over -- the denominator for
    /// `rejections`.
    pub entries_checked: usize,
    /// Programs excluded from that denominator because their retained set is
    /// not answerable by a declaration-based pass. `entries_checked +
    /// programs_unchecked == programs`, so a zero rejection count cannot hide
    /// a population that was never asked.
    pub programs_unchecked: usize,
    /// Registered programs -- the total the two above partition.
    pub programs: usize,
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
                format!("`{}`: {}", f.name, r.message)
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
        .programs
        .iter()
        .flat_map(|p| p.vert.into_iter().chain(std::iter::once(p.frag)))
        .map(|r| DeclKey(skull_mir::lower::canonical_variant(ws, r)))
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

    // THE PROFILE PASS IS EMISSION'S OWN, run per ENTRY.
    // `check_entry_profile` is the whole of what `emit_shader_entry` does
    // before it writes GLSL: stage classification of the entry's signature (an
    // `@uniform` block param is legal there and is NOT a tier-0 leaf -- asking
    // the fn-library check of an entry rejected 1,539 real shaders), the
    // entry's own lowering and tier-0 body pass, the same `Reach` walk over its
    // callees (which rejects recursion, a foreign body with no GLSL mapping and
    // a helper outside tier 0), and the stage-specific builtin validation.
    // A summary of those checks written here could disagree with the emitter;
    // this cannot.
    //
    // What the walk above still owns is PACKAGING -- which concrete instances
    // this artifact carries -- and the structural cycle finding that TYPES a
    // failure as recursion. The two are cross-checked rather than trusted: a
    // cycle the emitter accepts, or an emitter recursion rejection over an
    // acyclic retained relation, is a recorded `disagreements` entry.
    let cyclic = cyclic_instances(&roots, &edges);
    let mut rejections: Vec<ShaderRejection> = Vec::new();
    let mut disagreements: Vec<String> = Vec::new();
    let mut checked = 0usize;
    let mut unchecked_generic: BTreeSet<Instance> = BTreeSet::new();
    let mut missing_substitutions: BTreeSet<Instance> = BTreeSet::new();
    let mut programs_unchecked = 0usize;
    for program in req.programs {
        // The PROGRAM's own root instances -- the fragment entry, and the
        // vertex stage when the pair has one.
        let entry_of = |r: FuncRef| -> Option<&Instance> {
            let key = DeclKey(skull_mir::lower::canonical_variant(ws, r));
            roots.iter().find(|i| DeclKey(i.func) == key)
        };
        let Some(root) = entry_of(program.frag) else { continue };
        let mut program_roots: Vec<Instance> = vec![root.clone()];
        if let Some(v) = program.vert {
            match entry_of(v) {
                Some(i) => program_roots.push(i.clone()),
                // A registered vertex stage the graph holds no instance for is
                // not something this pass may check around.
                None => {
                    programs_unchecked += 1;
                    continue;
                }
            }
        }

        // THE COMPLETE RETAINED SET FOR THIS PROGRAM, not just its entries.
        // `check_entry_profile` lowers and walks the entry's HELPERS too, and
        // GLSL lowering takes no substitution -- so one generic helper anywhere
        // under the program makes the declaration-based verdict a statement
        // about a shape no artifact packages. The program is recorded unchecked
        // and the pass is not run: a verdict it cannot answer is not evidence,
        // and it may not enter the `entries_checked` denominator either.
        let retained = reachable_from(&program_roots, &edges);
        let mut answerable = true;
        for i in &retained {
            match g.concreteness(i) {
                Concreteness::Concrete => {}
                Concreteness::Generic => {
                    unchecked_generic.insert(i.clone());
                    answerable = false;
                }
                Concreteness::Missing => {
                    missing_substitutions.insert(i.clone());
                    answerable = false;
                }
            }
        }
        if !answerable {
            programs_unchecked += 1;
            continue;
        }

        checked += 1;
        let entry_cycle = cyclic.contains(root)
            || reaches_cycle(root, &edges, &cyclic);
        let outcome = match program.vert {
            Some(v) => skull_codegen_glsl::check_pipeline_profile(ws, v, program.frag),
            None => skull_codegen_glsl::check_entry_profile(ws, program.frag),
        };
        match outcome {
            Ok(()) => {
                if entry_cycle {
                    disagreements.push(format!(
                        "`{}`: the retained instance relation has a cycle the GLSL pass accepted",
                        ws.module(root.func.module).functions[root.func.func].name
                    ));
                }
            }
            Err(e) => {
                let f = &ws.module(root.func.module).functions[root.func.func];
                let span = e.labels.first().map(|l| l.span).unwrap_or(f.name_span);
                let kind = if entry_cycle {
                    ShaderRejectionKind::Recursion
                } else {
                    ShaderRejectionKind::Profile
                };
                if entry_cycle && !e.message.contains("recursion") {
                    disagreements.push(format!(
                        "`{}`: cyclic retained relation, but the GLSL pass rejected it as: {}",
                        f.name, e.message
                    ));
                }
                rejections.push(ShaderRejection {
                    instance: root.clone(),
                    kind,
                    message: e.message.clone(),
                    span,
                });
            }
        }
    }

    // ...and the same classification over the WHOLE retained set, so an
    // instance no registered program's walk reached (a second instantiation of
    // an entry declaration) is still reported rather than silently absent. The
    // per-program loop above decides what gets VALIDATED; this decides what
    // gets REPORTED, and the reported set is the larger of the two by
    // construction.
    for i in roots.iter().chain(helpers.iter()) {
        match g.concreteness(i) {
            Concreteness::Concrete => {}
            Concreteness::Generic => {
                unchecked_generic.insert(i.clone());
            }
            Concreteness::Missing => {
                missing_substitutions.insert(i.clone());
            }
        }
    }

    Some(ShaderArtifactView {
        profile,
        roots,
        helpers,
        builtins,
        rejections,
        unchecked_generic,
        missing_substitutions,
        disagreements,
        entries_checked: checked,
        programs_unchecked,
        programs: req.programs.len(),
        sites_walked,
    })
}

/// The instances reachable from `roots` over the RETAINED shader call relation
/// -- one program's own packaged set, taken from the same recorded edges the
/// whole-artifact walk built. Roots are included: an entry's own substitution
/// governs the declaration-based pass exactly as a helper's does.
fn reachable_from(
    roots: &[Instance],
    edges: &BTreeMap<Instance, Vec<(Instance, Span)>>,
) -> BTreeSet<Instance> {
    let mut seen: BTreeSet<Instance> = roots.iter().cloned().collect();
    let mut queue: VecDeque<Instance> = roots.iter().cloned().collect();
    while let Some(n) = queue.pop_front() {
        for (callee, _) in edges.get(&n).into_iter().flatten() {
            if seen.insert(callee.clone()) {
                queue.push_back(callee.clone());
            }
        }
    }
    seen
}

/// Whether `root` reaches any instance on a cycle.
fn reaches_cycle(
    root: &Instance,
    edges: &BTreeMap<Instance, Vec<(Instance, Span)>>,
    cyclic: &BTreeSet<Instance>,
) -> bool {
    let mut seen: BTreeSet<&Instance> = BTreeSet::new();
    let mut queue: VecDeque<&Instance> = VecDeque::new();
    queue.push_back(root);
    while let Some(n) = queue.pop_front() {
        if cyclic.contains(n) {
            return true;
        }
        for (callee, _) in edges.get(n).into_iter().flatten() {
            if seen.insert(callee) {
                queue.push_back(callee);
            }
        }
    }
    false
}

/// Instances on a cycle in the RETAINED shader call relation.
///
/// Over the recorded edges, not a second resolution of the program: the walk
/// above already asked what each site calls, and this only asks whether that
/// relation has a cycle. It does not decide the artifact's verdict --
/// `check_entry_profile` does -- it TYPES one, and the two are compared.
fn cyclic_instances(
    roots: &BTreeSet<Instance>,
    edges: &BTreeMap<Instance, Vec<(Instance, Span)>>,
) -> BTreeSet<Instance> {
    let mut out: BTreeSet<Instance> = BTreeSet::new();
    let mut done: BTreeSet<Instance> = BTreeSet::new();
    for root in roots {
        let mut path: Vec<(Instance, usize)> = vec![(root.clone(), 0)];
        let mut on_path: BTreeSet<Instance> = [root.clone()].into_iter().collect();
        while let Some((node, idx)) = path.pop() {
            let next = edges.get(&node).and_then(|es| es.get(idx)).cloned();
            match next {
                Some((callee, _)) => {
                    path.push((node, idx + 1));
                    if on_path.contains(&callee) {
                        out.insert(callee);
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
