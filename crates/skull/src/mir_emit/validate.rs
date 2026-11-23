use super::*;

/// Collected in assembly order, so parallel completion cannot change
/// diagnostic order.
#[derive(Default)]
pub(super) struct Rejections(Vec<CompilerError>);

impl Rejections {
    pub(super) fn push(&mut self, e: CompilerError) {
        self.0.push(e);
    }

    pub(super) fn extend(&mut self, es: impl IntoIterator<Item = CompilerError>) {
        self.0.extend(es);
    }

    pub(super) fn into_result(self) -> Result<(), CompilerError> {
        let mut it = self.0.into_iter();
        match it.next() {
            Some(primary) => Err(primary.with_siblings(it)),
            None => Ok(()),
        }
    }
}

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

#[derive(Default)]
pub(super) struct BodyRejection {
    findings: Vec<Finding>,
}

struct Finding {
    message: String,
    guarantee: bool,
    // Outranks the enclosing function's span: a whole-function anchor cannot tell
    // two violations in one body apart.
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

    /// One guarantee violation makes the whole rejection a guarantee, so a body
    /// holding both is never dropped on the strength of a gap.
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

pub(super) fn validate_emitted_body(
    ws: &Workspace,
    mf: &MirFunc,
    specs: &mut Vec<Instance>,
) -> Result<(), BodyRejection> {
    let mut acc = BodyRejection::default();
    // A demand the body cannot state self-contained is a rejection, not a dropped
    // demand whose call site would name a missing definition.
    if let Err(e) = skull_mir::reach::spec_bound_calls(ws, mf, specs) {
        acc.absorb(BodyRejection::gap(e.message));
    }
    if let Err(r) = validate_no_whole_element_column_access(mf).map_err(BodyRejection::gap) {
        acc.absorb(r);
    }
    acc.findings.extend(
        skull_transform::nobox_materialization_violations(ws, mf)
            .into_iter()
            .map(|(message, site)| Finding {
                // The span often points into an inlined stdlib body, so name the caller.
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

pub(super) fn check_unit_spec_bindings(
    ws: &Workspace,
    mid: ModuleId,
    name: &str,
    mf: &MirFunc,
    span: bone_semantic::prelude::Span,
    out_specs: &mut Vec<Instance>,
) -> Result<(), CompilerError> {
    match validate_emitted_body(ws, mf, out_specs) {
        Ok(()) => Ok(()),
        // Stdlib emit gaps are the known parity list; a user definition that cannot
        // emit is a hard error.
        Err(r) if !r.guarantee() && is_stdlib_module_file(ws.module(mid).file.as_str()) => Ok(()),
        Err(r) => Err(r.into_error(span, &format!("cannot emit `{name}`: "))),
    }
}
