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
) -> Result<Vec<Ustr>, BodyRejection> {
    let mut globals = GlobalRefs::default();
    validate_body(ws, mf, specs, Some(&mut globals))?;
    Ok(globals.names)
}

#[derive(Default)]
struct GlobalRefs {
    names: Vec<Ustr>,
    seen: FxHashSet<Ustr>,
}

fn validate_body(
    ws: &Workspace,
    mf: &MirFunc,
    specs: &mut Vec<Instance>,
    mut globals: Option<&mut GlobalRefs>,
) -> Result<(), BodyRejection> {
    use skull_mir::mir::ContainerKind::{VecValueColumns, VecValueStrided};
    let mut column = None;
    let mut has_ctors = false;
    for (_, block) in mf.blocks.iter() {
        for &iid in &block.insts {
            match &mf.insts[iid] {
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
                } => {
                    column.get_or_insert(e);
                }
                Inst::MakeValueStruct { .. } => has_ctors = true,
                Inst::Global(name) | Inst::StoreGlobal { name, .. } => {
                    if let Some(globals) = globals.as_deref_mut()
                        && globals.seen.insert(*name)
                    {
                        globals.names.push(*name);
                    }
                }
                _ => {}
            }
        }
    }
    let mut acc = BodyRejection::default();
    let mut spec_error = None;
    let has_specs = !mf.spec_bindings.is_empty();
    if has_specs || has_ctors {
        for bid in skull_mir::reach::emitted_blocks(mf) {
            for &iid in &mf.blocks[bid].insts {
                match mf.insts[iid] {
                    Inst::Call { .. } | Inst::CallMulti { .. }
                        if has_specs && spec_error.is_none() =>
                    {
                        match skull_mir::reach::spec_bound_call(ws, mf, iid) {
                            Ok(inst) => specs.extend(inst.cloned()),
                            Err(e) => spec_error = Some(e.message),
                        }
                    }
                    Inst::MakeValueStruct { name, .. } => {
                        if let Some((message, site)) =
                            skull_transform::nobox_materialization_violation(ws, mf, iid, name)
                        {
                            acc.findings.push(Finding {
                                // An inlined violation can point into a different source body.
                                message: format!("{message}, while emitting `{}`", mf.name),
                                guarantee: true,
                                site,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // Keep each diagnostic category in its original order, even though the
    // instruction walk collects them together.
    if column.is_some() || spec_error.is_some() {
        let mut gaps = BodyRejection::default();
        if let Some(message) = spec_error {
            gaps.absorb(BodyRejection::gap(message));
        }
        if let Some(elem) = column {
            gaps.absorb(BodyRejection::gap(format!(
                "whole-element access to a flat-stored `[{elem}]` survived lowering \
                 (column and strided storage have no element table, so there is no slot to name)"
            )));
        }
        gaps.absorb(acc);
        acc = gaps;
    }
    for nested in &mf.nested_fns {
        if let Err(rejection) = validate_body(ws, nested, specs, globals.as_deref_mut()) {
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
    match validate_body(ws, mf, out_specs, None) {
        Ok(()) => Ok(()),
        // Stdlib emit gaps are the known parity list; a user definition that cannot
        // emit is a hard error.
        Err(r) if !r.guarantee() && is_stdlib_module_file(ws.module(mid).file.as_str()) => Ok(()),
        Err(r) => Err(r.into_error(span, &format!("cannot emit `{name}`: "))),
    }
}
