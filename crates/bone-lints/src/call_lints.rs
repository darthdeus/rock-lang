use super::*;

pub const DEPRECATED: &str = "deprecated";

pub(super) fn deprecated_calls_in_body(
    ws: &Workspace,
    bi: Option<&BodyInference>,
    body: &Body,
) -> Vec<Finding> {
    let Some(bi) = bi else { return Vec::new() };
    let mut out = Vec::new();
    for (idx, &(mid, fid)) in &bi.resolved_overload_calls {
        let function = &ws.module(mid).functions[fid];
        let Some(annotation) = function
            .annotations
            .iter()
            .find(|a| a.is_builtin(DEPRECATED))
        else {
            continue;
        };
        let span = match body.expr_arena[*idx] {
            Expr::Call {
                callee: Callee::Unresolved { name_span, .. },
                ..
            } => body.name_spans[name_span],
            _ => body.expr_spans[*idx],
        };
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
    out
}
