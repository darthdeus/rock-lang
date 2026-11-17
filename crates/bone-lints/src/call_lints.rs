use super::*;

pub const STDLIB_UFCS: &str = "stdlib_ufcs";
pub const DEPRECATED: &str = "deprecated";

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
            let name = declared_name(function.name.as_str());
            let message = match replacement {
                Some(message) => format!("`{name}` is deprecated: {message}"),
                None => format!("`{name}` is deprecated"),
            };
            out.push(Finding::warning(DEPRECATED, span, message));
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
        out.push(Finding::advice(
            STDLIB_UFCS,
            span,
            format!("prefer receiver syntax `receiver.{method}(...)` over `{name}(receiver, ...)`"),
        ));
    }
    out
}
