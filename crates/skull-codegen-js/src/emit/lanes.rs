//! Multi-value lanes: publisher temps, return slots and the publisher rewrite.

use super::*;

/// The compiler-owned temp holding publisher argument `i` while the remaining
/// arguments evaluate. In the shared `__rock_` global space and outside the
/// reserved body namespace, like [`ret_slot`]; a body naming one rejects.
const PUBLISH_TEMP: &str = "__rock_lane";

fn publish_temp(i: usize) -> String {
    format!("{PUBLISH_TEMP}{i}")
}

/// Secondary return register `i` (1-based; leaf 0 rides the ordinary JS return
/// value). Module scope, not body-local, so it is outside the reserved
/// `r<N>`/`v<N>`/`c<N>` body namespace and cannot shadow a user global -- the
/// `__rock_` prefix is the shared compiler/stdlib global space.
pub(super) fn ret_slot(i: usize) -> String {
    format!("__rock_ret{i}")
}

/// `var __rock_ret1, ..;` for a program whose widest multi-value wire is `n`
/// leaves. Self-declaring per the runtime-globals doctrine: a caller assembling
/// a partial program without the prelude must not hit a ReferenceError.
pub(super) fn ret_slot_decls(n: usize) -> String {
    if n < 2 {
        return String::new();
    }
    let names: Vec<String> = (1..n).map(ret_slot).collect();
    format!("var {};\n", names.join(", "))
}

/// The widest result wire in `bodies`, as recorded by the owning ABI plans and,
/// for flat `ref` calls, by the lowering (`MirFunc::flat_call_lanes`).
/// Call destinations are operational storage; their length is not a second
/// ABI oracle.
///
/// MIR is not the only writer: an `inline(js)` body publishes its own lanes, so
/// the bank is sized over [`max_published_lanes`] too. Both paths that assemble
/// a program take that max -- sizing from MIR alone under-declares silently,
/// because the emitted program is sloppy-mode and an undeclared write just
/// becomes an implicit global.
pub(super) fn max_multi_wire(bodies: &[&MirFunc]) -> Result<usize, CompilerError> {
    pub(super) fn of(mf: &MirFunc) -> Result<usize, CompilerError> {
        let mut n = mf
            .abi_representation
            .as_ref()
            .map_or(0, skull_mir::lower::FunctionRepPlan::return_lanes);
        n = n.max(mf.abi_plan.as_ref().map_or(0, |plan| plan.result_lanes()));
        for (iid, inst) in mf.insts.iter() {
            match inst {
                Inst::CallMulti { .. } | Inst::CallValueMulti { .. } => {
                    // A flat `ref` wire first: a flat-result call also keeps
                    // its callee's declaration plan, which describes one lane.
                    if let Some(&lanes) = mf.flat_call_lanes.get(&iid) {
                        n = n.max(lanes);
                    } else if let Some(plan) = mf.call_abi_plans.get(&iid) {
                        n = n.max(plan.result_lanes());
                    } else if let Some(plan) = mf.call_rep_plans.get(&iid) {
                        n = n.max(plan.return_lanes());
                    } else {
                        return Err(err("JS: multi-value call has no owned ABI plan"));
                    }
                }
                Inst::CallBoundMulti { .. } => {
                    return Err(err(
                        "JS: unresolved bound multi-value call has no owned ABI plan",
                    ));
                }
                _ => {}
            }
        }
        for nested in &mf.nested_fns {
            n = n.max(of(nested)?);
        }
        Ok(n)
    }
    bodies
        .iter()
        .map(|mf| of(mf))
        .collect::<Result<Vec<_>, _>>()
        .map(|widths| widths.into_iter().max().unwrap_or(0))
}

/// The widest wire any reachable `inline(js)` body PUBLISHES, from the concrete
/// result ABI of each artifact-selected declaration -- never from the body text.
///
/// The bank has two writers: MIR ([`max_multi_wire`]) and the opaque bodies.
/// The opaque half used to be a substring scan for `__rock_retN`, which made
/// the transport whatever the string happened to say: a body could publish past
/// its own declared wire, or name a slot in a comment and widen the bank.
/// `BackendFnAbiPlan::result_lanes` is the same ABI policy every
/// definition and call site reads, so a publisher and the caller capturing it
/// cannot disagree.
pub(super) fn max_published_lanes(
    view: &ArtifactView,
    inline_js: &[JsInlineBody],
) -> Result<usize, CompilerError> {
    inline_js
        .iter()
        .map(|body| {
            let Some(realized) = view.realization.get(&body.instance) else {
                return Err(err(format!(
                    "JS: inline body `{:?}` has no artifact realization",
                    body.decl
                )));
            };
            if !matches!(
                realized.kind,
                artifact::Realization::ForeignBody(
                    skull_graph::instance_graph::BoundaryClass::InlineBody
                )
            ) {
                return Ok(0);
            }
            if let Some(&lanes) = view.publisher_lanes.get(&body.instance) {
                return Ok(lanes);
            }
            Err(err(format!(
                "JS: inline body `{:?}` has no artifact publisher lane plan",
                body.decl,
            )))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|lanes| lanes.into_iter().max().unwrap_or(0))
}

/// `body` with every comment and string/template literal replaced by spaces of
/// the same length: byte offsets are preserved, so a lexical check can look at
/// CODE only and still slice the original text.
///
/// Plain substring matching is not an oracle here -- a `__rock_ret1` inside a
/// comment is not bank access, and a `,` inside a string is not an argument
/// separator. An unrecognized shape must REJECT rather than be guessed at,
/// which is what the caller does with what this returns.
fn code_only(body: &str) -> String {
    let b = body.as_bytes();
    let mut out = vec![b' '; b.len()];
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            q @ (b'"' | b'\'' | b'`') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            c => {
                out[i] = c;
                i += 1;
            }
        }
    }
    // Newlines survive blanking so line structure (and `//` termination) is
    // unchanged for anything reading this back.
    for (i, c) in body.bytes().enumerate() {
        if c == b'\n' {
            out[i] = b'\n';
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Identifier occurrences of `name` in `code` (word-bounded), as byte offsets.
fn ident_positions(code: &str, name: &str) -> Vec<usize> {
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = code[from..].find(name) {
        let at = from + rel;
        from = at + name.len();
        let before_ok = at == 0 || !is_word(b[at - 1]);
        let after = at + name.len();
        let after_ok = after >= b.len() || !is_word(b[after]);
        if before_ok && after_ok {
            out.push(at);
        }
    }
    out
}

/// The lowered form of a multi-lane `inline(js)` body, or the reason its
/// publisher does not match the contract.
///
/// The contract: a multi-lane
/// body contains exactly one final top-level `return __rock_publish(...)` with
/// one argument per ABI result lane, no second `return`, and no direct scratch
/// bank access. A single-lane body returns ordinarily and may use neither.
pub(super) fn lower_publisher(
    body: &str,
    lanes: usize,
    name: &str,
) -> Result<String, CompilerError> {
    let code = code_only(body);
    if code.contains(PUBLISH_TEMP) {
        return Err(err(format!(
            "`inline(js)` body of `{name}` names `{PUBLISH_TEMP}N`, which is compiler-owned: \
             the publisher lowering evaluates its arguments into those temps"
        )));
    }
    if code.contains("__rock_ret") {
        return Err(err(format!(
            "`inline(js)` body of `{name}` touches the scratch bank directly; a multi-lane \
             result publishes through one final `return __rock_publish(..)`"
        )));
    }
    let publishes = ident_positions(&code, "__rock_publish");
    let returns = ident_positions(&code, "return");
    if lanes < 2 {
        if !publishes.is_empty() {
            return Err(err(format!(
                "`inline(js)` body of `{name}` publishes to the scratch bank, but its result \
                 ABI has {lanes} result lane -- an ordinary `return` carries it"
            )));
        }
        return Ok(body.to_string());
    }
    let [publish_at] = publishes[..] else {
        return Err(err(format!(
            "`inline(js)` body of `{name}` has {lanes} result lanes and needs exactly one \
             final `return __rock_publish(..)`; found {} occurrences of `__rock_publish`",
            publishes.len()
        )));
    };
    let [return_at] = returns[..] else {
        return Err(err(format!(
            "`inline(js)` body of `{name}` must have one final `return __rock_publish(..)` \
             and no other `return`; found {} `return` statements",
            returns.len()
        )));
    };
    // `return` immediately followed by the publisher, and nothing but the call
    // between them. A single-exit body by construction.
    if return_at > publish_at
        || !code[return_at + "return".len()..publish_at]
            .trim()
            .is_empty()
    {
        return Err(err(format!(
            "`inline(js)` body of `{name}` must end with one final \
             `return __rock_publish(..)`; its `return` does not name the publisher"
        )));
    }
    let open = code[publish_at..]
        .find('(')
        .map(|r| publish_at + r)
        .ok_or_else(|| {
            err(format!(
                "`inline(js)` body of `{name}`: `__rock_publish` is not a call"
            ))
        })?;
    // Argument boundaries come from the BLANKED text (a comma inside a string is
    // not a separator); the argument TEXT comes from the original body.
    let mut depth = 0usize;
    let mut splits: Vec<usize> = vec![open + 1];
    let mut close = None;
    for (i, c) in code[open..].char_indices() {
        let at = open + i;
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(at);
                    break;
                }
            }
            ',' if depth == 1 => splits.push(at + 1),
            _ => {}
        }
    }
    let close = close.ok_or_else(|| {
        err(format!(
            "`inline(js)` body of `{name}`: unbalanced `__rock_publish(..)`"
        ))
    })?;
    if !code[close + 1..]
        .trim()
        .trim_end_matches(';')
        .trim()
        .is_empty()
    {
        return Err(err(format!(
            "`inline(js)` body of `{name}`: the publisher must be the body's final statement"
        )));
    }
    let mut args: Vec<&str> = Vec::new();
    for (n, &start) in splits.iter().enumerate() {
        let end = splits.get(n + 1).map(|next| next - 1).unwrap_or(close);
        args.push(body[start..end].trim());
    }
    if args.len() == 1 && args[0].is_empty() {
        args.clear();
    }
    if args.len() != lanes {
        return Err(err(format!(
            "`inline(js)` body of `{name}` publishes {} values, but its concrete result ABI \
             has {lanes} result lanes",
            args.len()
        )));
    }
    // ORDER IS PART OF THE CONTRACT. `__rock_publish(f(), g())` is an ordinary
    // JS call expression, so its arguments evaluate left to right; assigning
    // lane 1 from `g()` first would run `g` before `f` and publish a lane
    // before `f` had a chance to throw. Every argument is therefore evaluated,
    // IN ORDER, into compiler-owned temps, and only then does anything reach
    // the bank -- which is also what makes a throwing publisher leave no lane
    // behind for the caller (which never captures) to see later.
    let prefix = &body[..return_at];
    let mut lowered = String::from(prefix);
    let temps: Vec<String> = (0..args.len()).map(publish_temp).collect();
    lowered.push_str("var ");
    for (i, (t, a)) in temps.iter().zip(args.iter()).enumerate() {
        if i > 0 {
            lowered.push_str(", ");
        }
        lowered.push_str(&format!("{t} = {a}"));
    }
    lowered.push_str("; ");
    for (i, t) in temps.iter().enumerate().skip(1) {
        lowered.push_str(&format!("{} = {t}; ", ret_slot(i)));
    }
    lowered.push_str(&format!("return {};", temps[0]));
    Ok(lowered)
}
