//! Rejection of bodies and units that fail post-emit validation.

use super::*;

/// Every rejection one assembly found, across all four kinds of emitted
/// definition (decorator body, global initializer, function chunk, specialized
/// instantiation). Filled in a fixed order -- source order within each kind, the
/// kinds in the order `assemble` visits them -- so the reported list never
/// depends on thread scheduling or hash-map iteration.
///
/// The accumulation boundary is the DEFINITION: each one either emits or does
/// not, and rejections within one body are already collected by
/// [`BodyRejection`]. Aborting at the first definition is what made every count
/// of remaining rejections a floor -- every rejection here is fully computed
/// before any is reported, so the abort hid them rather than preventing them.
/// Accumulating does not soften anything: a non-empty set fails the build, and
/// the emitted Lua is discarded with the `Err`.
#[derive(Default)]
pub(super) struct Rejections(Vec<CompilerError>);

impl Rejections {
    pub(super) fn push(&mut self, e: CompilerError) {
        self.0.push(e);
    }

    pub(super) fn extend(&mut self, es: impl IntoIterator<Item = CompilerError>) {
        self.0.extend(es);
    }

    /// The first rejection, carrying the rest as siblings (the shared sink every
    /// renderer expands). `Ok` only when nothing was rejected.
    pub(super) fn into_result(self) -> Result<(), CompilerError> {
        let mut it = self.0.into_iter();
        match it.next() {
            Some(primary) => Err(primary.with_siblings(it)),
            None => Ok(()),
        }
    }
}

/// Post-transform gate: no whole-element access to a column-stored Vec may reach
/// the emitter. Column storage has no element table, so there is no single slot
/// to read or write; lowering decomposes every such access into per-column ones
/// (`column_element_repr` / `column_element_store`). Anything left is a
/// decomposition the lowerer does not implement yet -- today, a stdlib Vec entry
/// point that still goes through the boxed `_data` (`enumerate` is the one real
/// game code hits, in an application's update loop).
///
/// The emitter already refuses these (`write_index`'s `unreachable!`), but a
/// panic in a parallel emit worker is the wrong shape of loud: no span, no source
/// location, and the process exit code depends on the caller. Rejecting here
/// gives the same guarantee as a spanned `CompilerError`.
fn validate_no_whole_element_column_access(mf: &skull_mir::mir::MirFunc) -> Result<(), String> {
    use skull_mir::mir::{
        ContainerKind::{VecValueColumns, VecValueStrided},
        Inst,
    };
    for (_, block) in mf.blocks.iter() {
        for &iid in &block.insts {
            let elem = match &mf.insts[iid] {
                Inst::Index {
                    kind: VecValueColumns(e) | VecValueStrided(e),
                    ..
                }
                | Inst::StoreIndex {
                    kind: VecValueColumns(e) | VecValueStrided(e),
                    ..
                }
                | Inst::ArrayPush {
                    kind: VecValueColumns(e) | VecValueStrided(e),
                    ..
                } => e,
                _ => continue,
            };
            return Err(format!(
                "whole-element access to a flat-stored `[{elem}]` survived lowering \
                 (column and strided storage have no element table, so there is no slot to name)"
            ));
        }
    }
    Ok(())
}

/// [`validate_emitted_body`] over a hoisted-body tree (named nested `fn`s and the
/// lambda bodies `convert_closures` appends), each of which emits as its own
/// top-level Lua function and so needs the checks in its own right.
fn validate_nested_bodies(
    ws: &Workspace,
    ctx: &skull_mir::bound::BoundCtx,
    nested: &[MirFunc],
    specs: &mut Vec<skull_mir::bound::SpecDemand>,
) -> Result<(), BodyRejection> {
    let mut acc = BodyRejection::default();
    for nf in nested {
        if let Err(r) = validate_emitted_body(ws, ctx, nf, specs) {
            acc.absorb(r.prefixed(&format!("in nested body `{}`", nf.name)));
        }
    }
    acc.into_result()
}

/// Why an about-to-be-emitted body was rejected: every INDEPENDENT finding in
/// it, first reported as the primary and the rest as siblings. A body holds
/// several sinks often enough that reporting one turns any count of remaining
/// sinks into a floor, and it can hold several KINDS of finding at once (a
/// column-access gap and a `nobox` sink), which reporting only the first check
/// to fire also hides.
#[derive(Default)]
pub(super) struct BodyRejection {
    findings: Vec<Finding>,
}

/// One rejection site. `guarantee` separates a LANGUAGE GUARANTEE (today:
/// `nobox`) from a codegen parity gap. `site` is the offending instruction's own span when the check could
/// name one; it outranks the enclosing fn's span at [`BodyRejection::into_error`]
/// -- a whole-fn anchor cannot distinguish two violations in one body, which is
/// how one `nobox` sink hid another inside `draw_fractures`.
struct Finding {
    message: String,
    guarantee: bool,
    site: Option<bone_semantic::prelude::Span>,
}

impl BodyRejection {
    fn gap(message: String) -> Self {
        Self {
            findings: vec![Finding {
                message,
                guarantee: false,
                site: None,
            }],
        }
    }

    /// A guarantee violation anywhere in the body makes the whole rejection a
    /// guarantee: the stdlib path may drop a body with only gaps in it, and a
    /// body holding both must not be dropped on the strength of the gap.
    fn guarantee(&self) -> bool {
        self.findings.iter().any(|f| f.guarantee)
    }

    fn absorb(&mut self, other: BodyRejection) {
        self.findings.extend(other.findings);
    }

    fn into_result(self) -> Result<(), BodyRejection> {
        if self.findings.is_empty() {
            Ok(())
        } else {
            Err(self)
        }
    }

    fn prefixed(mut self, what: &str) -> Self {
        for f in &mut self.findings {
            f.message = format!("{what}: {}", f.message);
        }
        self
    }

    /// Spanned rejection at the offending instruction, falling back to `span`
    /// (the enclosing definition), tagged so the stdlib leniency check can tell
    /// a guarantee from a gap without matching on the message text.
    pub(super) fn into_error(
        self,
        span: bone_semantic::prelude::Span,
        what: &str,
    ) -> CompilerError {
        let mut errs = self.findings.into_iter().map(|f| {
            let mut e = CompilerError::new_with_label(
                f.site.unwrap_or(span),
                format!("{what}{}", f.message),
            );
            if f.guarantee {
                e.kind = skull_parser::CompilerErrorKind::NoboxGuaranteeViolated;
            }
            e
        });
        let primary = errs.next().expect("a rejection holds at least one finding");
        primary.with_siblings(errs)
    }
}

/// Every post-transform invariant one about-to-be-emitted body must hold, and its
/// hoisted children with it, plus the specialized callees it needs emitted. The
/// single entry point on purpose: each check below was added because some
/// emitted-body kind was missing it, so a new one joins here rather than at one
/// call site.
pub(super) fn validate_emitted_body(
    ws: &Workspace,
    ctx: &skull_mir::bound::BoundCtx,
    mf: &MirFunc,
    specs: &mut Vec<skull_mir::bound::SpecDemand>,
) -> Result<(), BodyRejection> {
    let mut acc = BodyRejection::default();
    // A demand this body cannot state in self-contained terms is a rejection,
    // not a demand that is quietly dropped: the callee would then be reached by
    // nothing and its call site would spell a definition the program lacks.
    if let Err(e) = skull_mir::reach::spec_bound_calls(ws, mf, specs) {
        acc.absorb(BodyRejection::gap(e.message));
    }
    if let Err(r) = validate_no_whole_element_column_access(mf).map_err(BodyRejection::gap) {
        acc.absorb(r);
    }
    // Name the function being emitted: the span often points into an INLINED
    // stdlib body (every `xs.push(..)` reports at the same `Vec<T>.push` line),
    // so without the caller the diagnostic cannot tell two sites apart.
    acc.findings.extend(
        skull_transform::nobox_materialization_violations(ws, mf)
            .into_iter()
            .map(|(message, site)| Finding {
                message: format!("{message}, while emitting `{}`", mf.name),
                guarantee: true,
                site,
            }),
    );
    if let Err(r) = validate_nested_bodies(ws, ctx, &mf.nested_fns, specs) {
        acc.absorb(r);
    }
    acc.into_result()
}

/// The spec-binding check for a decorator / global-init unit, whose body and
/// hoisted lambdas are emitted without ever passing through the inliner. Lenient
/// for stdlib modules on the same grounds as the fn path: their emit gaps are the
/// known parity list, a user definition that cannot emit is a hard error.
pub(super) fn check_unit_spec_bindings(
    ws: &Workspace,
    ctx: &skull_mir::bound::BoundCtx,
    mid: ModuleId,
    name: &str,
    mf: &MirFunc,
    span: bone_semantic::prelude::Span,
    out_specs: &mut Vec<skull_mir::bound::SpecDemand>,
) -> Result<(), CompilerError> {
    let mut specs = Vec::new();
    let r = validate_emitted_body(ws, ctx, mf, &mut specs);
    out_specs.extend(specs);
    match r {
        Ok(()) => Ok(()),
        Err(r) if !r.guarantee() && is_stdlib_module_file(ws.module(mid).file.as_str()) => Ok(()),
        Err(r) => Err(r.into_error(span, &format!("cannot emit `{name}`: "))),
    }
}

/// EVERY [`UnitOut::hard_error`] in a parallel batch, in source order so the
/// reported list does not depend on thread scheduling. Each unit is an
/// independent definition, so a batch of them holds a site table, not one bug.
pub(super) fn unit_errors(units: &[UnitOut]) -> Vec<CompilerError> {
    units.iter().filter_map(|u| u.hard_error.clone()).collect()
}
