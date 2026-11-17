//! Rejection of bodies and units that fail post-emit validation.

use super::*;

/// Collect definition errors in assembly order so parallel completion cannot
/// change diagnostic order.
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

/// A column-backed vector has no whole-element slot to address.
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

/// Retain all independent findings from validating a body.
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
    for nested in &mf.nested_fns {
        if let Err(rejection) = validate_emitted_body(ws, nested, specs) {
            acc.absorb(rejection.prefixed(&format!("in nested body `{}`", nested.name)));
        }
    }
    acc.into_result()
}

/// The spec-binding check for a decorator / global-init unit, whose body and
/// hoisted lambdas are emitted without ever passing through the inliner. Lenient
/// for stdlib modules on the same grounds as the fn path: their emit gaps are the
/// known parity list, a user definition that cannot emit is a hard error.
pub(super) fn check_unit_spec_bindings(
    ws: &Workspace,
    mid: ModuleId,
    name: &str,
    mf: &MirFunc,
    span: bone_semantic::prelude::Span,
    out_specs: &mut Vec<skull_mir::bound::SpecDemand>,
) -> Result<(), CompilerError> {
    match validate_emitted_body(ws, mf, out_specs) {
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
