use super::*;

pub const STDLIB_UFCS: &str = "stdlib_ufcs";
pub const DEPRECATED: &str = "deprecated";

struct StdlibUfcs {
    name: Ustr,
    method: Ustr,
    span: Span,
}

impl LintFinding for StdlibUfcs {
    const LINT: &'static str = STDLIB_UFCS;
    const LEVEL: CompilerErrorLevel = CompilerErrorLevel::Advice;

    fn span(&self) -> Span {
        self.span
    }

    fn message(&self) -> String {
        format!(
            "prefer receiver syntax `receiver.{}(...)` over `{}(receiver, ...)`",
            self.method, self.name
        )
    }
}

struct Deprecated {
    name: Ustr,
    replacement: Option<Ustr>,
    span: Span,
}

impl LintFinding for Deprecated {
    const LINT: &'static str = DEPRECATED;
    const LEVEL: CompilerErrorLevel = CompilerErrorLevel::Warning;

    fn span(&self) -> Span {
        self.span
    }

    fn message(&self) -> String {
        let name = declared_name(self.name.as_str());
        match self.replacement {
            Some(message) => format!("`{name}` is deprecated: {message}"),
            None => format!("`{name}` is deprecated"),
        }
    }
}

pub(super) fn findings(
    ws: &Workspace,
    bi: Option<&BodyInference>,
    body: &Body,
    sel: &LintOptions,
) -> Vec<Finding> {
    let Some(bi) = bi else { return Vec::new() };
    let mut out = Vec::new();
    for (idx, &(mid, fid)) in &bi.resolved_overload_calls {
        let module = ws.module(mid);
        let function = &module.functions[fid];
        let span = match body.expr_arena[*idx] {
            Expr::Call {
                callee: Callee::Unresolved { name_span, .. },
                ..
            } => body.name_spans[name_span],
            _ => body.expr_spans[*idx],
        };
        if sel.enabled(DEPRECATED)
            && let Some(annotation) = function
                .annotations
                .iter()
                .find(|a| a.is_builtin(DEPRECATED))
        {
            let replacement = annotation.args.first().and_then(|arg| match arg.value {
                AnnoValue::String(message) => Some(message),
                _ => None,
            });
            out.push(Finding::of(&Deprecated {
                name: function.name,
                replacement,
                span,
            }));
        }
        if !sel.enabled(STDLIB_UFCS)
            || !bone_core::modules::is_stdlib_module_file(module.file.as_str())
        {
            continue;
        }
        let Expr::Call { args, .. } = body.expr_arena[*idx] else {
            continue;
        };
        if args.len() == 0
            || args.into_iter().any(|arg| {
                body.method_call_recvs.contains(&arg)
                    || body.arg_names.contains_key(&arg)
                    || body.spread_args.contains(&arg)
            })
        {
            continue;
        }
        let name = function.name.as_str();
        let method = match name {
            "vec_push" => "push",
            "vec_get" => "get",
            "vec_set" => "set",
            "vec_len" => "len",
            _ if name.starts_with("string_")
                && function
                    .param_decls
                    .first()
                    .is_some_and(|p| p.ty == ws.builtins.string()) =>
            {
                name
            }
            _ => continue,
        };
        out.push(Finding::of(&StdlibUfcs {
            name: function.name,
            method: Ustr::from(method),
            span,
        }));
    }
    out
}
