//! Completion candidate selection + materialization (completion-v2 stages
//! B/D): the fact-shaped `Cand`/`Sink`, the ONE candidate ordering, the
//! diversity pass, and `materialize_cand` -- the only place display strings
//! are built. Tuning constants live here with the pipeline they cap.

use std::collections::HashMap;

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, InsertTextFormat,
};

use crate::query::SymCat;
use crate::rank_key::RankKey;

use super::rank::ci_cmp;
use super::sources::ArgPayload;

/// Strip a trailing `name($0)` / `name()` CALL template to the bare name when
/// the cursor is already directly before a `(` -- reusing the existing parens
/// instead of doubling them (`world_to_design()(x)`). Only touches call
/// templates: `{ $0 }` record/block and `<$0>` generic templates end in `}`/`>`
/// and a filled call-through (`foo(a, b)`) ends in `)` preceded by an arg, so
/// none of them match the exact `($0)`/`()` suffix.
pub(crate) fn strip_call_template(items: &mut [CompletionItem]) {
    for it in items.iter_mut() {
        let Some(t) = it.insert_text.as_deref() else { continue };
        if let Some(bare) = t.strip_suffix("($0)").or_else(|| t.strip_suffix("()")) {
            it.insert_text = Some(bare.to_string());
            it.insert_text_format = None;
        }
    }
}

// === Completion tuning (docs/completion-v2-design.md) =======================
// Every cap/threshold the ranking pipeline exposes, in one block. The fuzzy
// score weights (FZ_*) live next to the DP above; the RankKey field order is
// the ladder itself (rank_key.rs).

/// Default cap on items returned by general (non-member) completion. Generous
/// headroom over a screenful -- the client narrows within a keystroke and
/// re-requests (every completion response is `isIncomplete`), so this exists
/// only to stop serializing the whole workspace (2539 items / ~518 KB on mag at
/// empty prefix) when the client will throw nearly all of it away and ask again.
/// Override at runtime with `ROCK_LSP_MAX_COMPLETIONS` (see `max_completion_items`).
pub(crate) const DEFAULT_MAX_COMPLETION_ITEMS: usize = 200;

/// The effective general-completion cap: `ROCK_LSP_MAX_COMPLETIONS` if it parses
/// to a positive integer, else `DEFAULT_MAX_COMPLETION_ITEMS`. Read once (the env
/// is process-stable), mirroring `lsp_profile_enabled`.
pub(crate) fn max_completion_items() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| parse_max_completions(std::env::var("ROCK_LSP_MAX_COMPLETIONS").ok().as_deref()))
}

/// Parse the `ROCK_LSP_MAX_COMPLETIONS` value: a positive integer wins, anything
/// else (unset, empty, non-numeric, 0) falls back to the default. Pure so the
/// override policy is testable without touching process env.
fn parse_max_completions(v: Option<&str>) -> usize {
    v.and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_COMPLETION_ITEMS)
}

/// Max reach paths expanded per ROOT toward the expected type -- a principled
/// "k shortest ways" (ranked by distance), not a depth limit. Shared by the
/// general arg-position expansion and the member transitive-path expansion.
pub(crate) const REACH_CAP: usize = 64;

/// Frequency prior buckets (`RankKey::freq`): >= HOT refs -> 0, >= WARM ->
/// 1, else 2. Placeholder thresholds until the M6 replay corpus tunes them.
pub(crate) const FREQ_HOT: u32 = 16;
pub(crate) const FREQ_WARM: u32 = 3;

/// Call-through: max synthesized `f(arg)` items kept overall, and max
/// METHOD-CHAIN args kept per fn (direct values / field paths are uncapped so
/// sibling fields stay browsable; the global cap backstops the total).
pub(crate) const CALL_THROUGH_CAP: usize = 10;
pub(crate) const PER_FN_METHOD_CAP: usize = 2;

// === Completion candidate pipeline (docs/completion-v2-design.md, M3) ========
//
// Stage B/D of completion-v2: every general-path source emits fact-shaped
// `Cand`s into a `Sink`; ONE select (RankKey order) runs on the facts;
// `CompletionItem`s (labels, inserts, details) materialize in
// `materialize_cand` for the post-cap survivors only. Sources never build
// display strings for ranking, and no post-pass reverse-engineers structure
// from labels. The response array is emitted in sort_text order (the
// invariant every path upholds; `bone_query::invariants`) -- there is no
// post-sort reordering pass (`diversify` was deleted: it reordered only the
// array, which the real client sorts dead-last, so it was a live no-op --
// unfuck-backlog #30 resolution, `cdac37939`).

/// The name a candidate ranks and sorts under -- the string `RankKey::encode`
/// appends and the ci-alphabetical tiebreak compares. NOT always the display
/// label: a named-arg `greeting:` ranks under `greeting`, a reach item
/// labeled `player.grid: IVec2` under `player.grid`.
pub(crate) enum CandName {
    Sym(ustr::Ustr),
    Owned(String),
}

impl CandName {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            CandName::Sym(u) => u.as_str(),
            CandName::Owned(s) => s.as_str(),
        }
    }
}

/// Materialization facts: everything `materialize_cand` needs to render a
/// survivor's label / insert / detail / kind.
pub(crate) enum CandShape {
    /// A scope/global-namespace symbol from `crate::query::completion`.
    Entry(crate::query::CompletionEntry),
    /// Param-name + fn-prefix `Entity<..>` type synth: a known payload
    /// renders concrete `Entity<Mob>`, an unknown one the `Entity<$0>`
    /// snippet.
    EntitySynth { payload: Option<ustr::Ustr> },
    /// A callee param offered as `name:` at a direct call-arg slot.
    NamedArg { pname: ustr::Ustr, pty: String },
    /// `CoFrame` in the first generic slot of `Coroutine<..>`.
    CoFrame,
    /// `Coroutine<CoFrame>` / `Coroutine<$0>` replacing the bare type name.
    Coroutine { with_coframe: bool },
    /// `-> Coroutine<..>` return type offered after `co fn name()`.
    CoReturn { coframe: bool },
    /// A reachability path; the cand name is the access form `root.suffix`.
    Reach { final_ty: String, is_method: bool, takes_args: bool },
    /// A synthesized whole call; the cand name is `fname(arg)`.
    CallThrough { fname: ustr::Ustr, arg: String, payload: ArgPayload, multiparam: bool },
    /// A `deco` declaration (decorator position).
    Decorator { detail: Option<String> },
}

/// A fact-shaped completion candidate: rank key + rank/tiebreak name +
/// materialization facts. `key` is stored CLAMPED (`RankKey::clamped`) so
/// `cand_cmp` and the client's sort_text string order agree at any field
/// value.
pub(crate) struct Cand {
    pub(crate) key: RankKey,
    pub(crate) name: CandName,
    pub(crate) shape: CandShape,
}

impl Cand {
    pub(crate) fn new(key: RankKey, name: CandName, shape: CandShape) -> Self {
        Cand { key: key.clamped(), name, shape }
    }
}

/// Stage-B sink: sources emit `Cand`s here in source order. Owns the
/// cross-source dedup namespace (reach access forms and call-through call
/// strings share it) and the synth-shadowing retain (a synth item
/// superseding the same-named workspace symbol).
pub(crate) struct Sink {
    pub(crate) cands: Vec<Cand>,
    pub(crate) seen: std::collections::HashSet<String>,
}

impl Sink {
    pub(crate) fn new() -> Self {
        Sink { cands: Vec::new(), seen: std::collections::HashSet::new() }
    }

    pub(crate) fn push(&mut self, cand: Cand) {
        self.cands.push(cand);
    }

    /// Admit `name` once across every deduping source: true the first time,
    /// false for a duplicate (the caller skips the emit).
    pub(crate) fn admit(&mut self, name: &str) -> bool {
        if self.seen.contains(name) {
            return false;
        }
        self.seen.insert(name.to_string());
        true
    }

    /// Drop an already-emitted candidate a synth supersedes (e.g. the plain
    /// `CoFrame` struct symbol when the coroutine-slot synth takes over).
    pub(crate) fn retain_not_named(&mut self, name: &str) {
        self.cands.retain(|c| c.name.as_str() != name);
    }

    pub(crate) fn len(&self) -> usize {
        self.cands.len()
    }
}

/// The ONE candidate ordering: clamped `RankKey`, then the ci name tiebreak
/// -- by construction the order the client's `sort_text` string sort
/// produces (`RankKey::encode` appends the lowercased name).
pub(crate) fn cand_cmp(a: &Cand, b: &Cand) -> std::cmp::Ordering {
    a.key
        .cmp(&b.key)
        .then_with(|| ci_cmp(a.name.as_str().as_bytes(), b.name.as_str().as_bytes()))
}

/// Stage D: render ONE surviving candidate to its `CompletionItem`. Every
/// display string (label, insert, detail, sort_text) is built here and only
/// here. `strip_call` = the cursor sits directly before an existing `(`
/// (e.g. `cw` over the callee of `world_to_screen(x)`): call-template
/// shapes insert the bare name so the existing parens are reused instead of
/// doubled. `pattern` = the cursor sits in a match-arm pattern HEAD:
/// patterns bind, they don't construct, so a variant inserts its bare
/// qualified name and never the payload template (the same rule the
/// `Enum::|` qualified path applies).
pub(crate) fn materialize_cand(
    cand: Cand,
    fn_sigs: &HashMap<ustr::Ustr, String>,
    strip_call: bool,
    pattern: bool,
) -> CompletionItem {
    let sort_text = Some(cand.key.encode(cand.name.as_str()));
    match cand.shape {
        CandShape::Entry(c) => {
            // Function `detail` is the cached signature, materialized HERE
            // for survivors only; non-fn kinds carry their (cheap) detail
            // already.
            let detail = match c.kind {
                SymCat::Function => fn_sigs.get(&c.name).cloned(),
                _ => c.detail,
            };
            // The full detail goes in the panel; a concise form renders
            // inline next to the label in the popup (a fn's `(params) ->
            // ret`, a binding's `: type`).
            let label_details = fn_or_type_label_details(c.kind, detail.as_deref());
            // A function with params accepts as a snippet `name($0)` ->
            // inserts `name()` with the cursor parked between the parens
            // (the client shows a snippet marker, e.g. nvim-cmp's `~`). A
            // param-less function inserts `name()` as plain text so the
            // cursor lands AFTER the `)` -- there's nothing to fill.
            // A qualified `Enum::Variant` value in an expression slot
            // carries the same payload template as the `Enum::` path:
            // tuple -> `($0)`, record -> `{ $0 }`, unit -> bare.
            let (insert_text, insert_text_format) = match c.kind {
                // `on_killed`/`on_resume`/`on_hot_reload` are contextual
                // lifecycle-hook keywords with a statement BLOCK form
                // (`on_killed { .. }`) -- complete to the block, not a call.
                SymCat::Function if is_lifecycle_hook(c.name.as_str()) => {
                    (Some(format!("{} {{ $0 }}", c.name)), Some(InsertTextFormat::SNIPPET))
                }
                SymCat::Function if strip_call => (Some(c.name.to_string()), None),
                SymCat::Function if c.takes_params => {
                    (Some(format!("{}($0)", c.name)), Some(InsertTextFormat::SNIPPET))
                }
                SymCat::Function => (Some(format!("{}()", c.name)), None),
                SymCat::Variant if pattern => (None, None),
                SymCat::Variant => match c.variant_shape {
                    Some(crate::query::VariantShape::Tuple) if strip_call => {
                        (Some(c.name.to_string()), None)
                    }
                    Some(crate::query::VariantShape::Tuple) => {
                        (Some(format!("{}($0)", c.name)), Some(InsertTextFormat::SNIPPET))
                    }
                    Some(crate::query::VariantShape::Record) => {
                        (Some(format!("{} {{ $0 }}", c.name)), Some(InsertTextFormat::SNIPPET))
                    }
                    _ => (None, None),
                },
                _ => (None, None),
            };
            CompletionItem {
                label: c.name.to_string(),
                kind: Some(completion_kind(c.kind)),
                label_details,
                detail,
                insert_text,
                insert_text_format,
                sort_text,
                ..Default::default()
            }
        }
        CandShape::EntitySynth { payload } => {
            let (label, insert, fmt) = match payload {
                Some(p) => (format!("Entity<{p}>"), format!("Entity<{p}>"), None),
                None => (
                    "Entity<>".to_string(),
                    "Entity<$0>".to_string(),
                    Some(InsertTextFormat::SNIPPET),
                ),
            };
            CompletionItem {
                label,
                kind: Some(CompletionItemKind::STRUCT),
                insert_text: Some(insert),
                insert_text_format: fmt,
                sort_text,
                ..Default::default()
            }
        }
        CandShape::NamedArg { pname, pty } => CompletionItem {
            label: format!("{pname}:"),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(pty),
            insert_text: Some(format!("{pname}: ")),
            sort_text,
            ..Default::default()
        },
        CandShape::CoFrame => CompletionItem {
            label: "CoFrame".to_string(),
            kind: Some(CompletionItemKind::STRUCT),
            sort_text,
            ..Default::default()
        },
        CandShape::Coroutine { with_coframe } => {
            let (label, insert, fmt) = if with_coframe {
                ("Coroutine<CoFrame>", "Coroutine<CoFrame>", None)
            } else {
                ("Coroutine<>", "Coroutine<$0>", Some(InsertTextFormat::SNIPPET))
            };
            CompletionItem {
                label: label.to_string(),
                kind: Some(CompletionItemKind::STRUCT),
                insert_text: Some(insert.to_string()),
                insert_text_format: fmt,
                sort_text,
                ..Default::default()
            }
        }
        CandShape::CoReturn { coframe } => {
            let (label, insert, fmt) = if coframe {
                ("-> Coroutine<CoFrame>", "-> Coroutine<CoFrame>", None)
            } else {
                ("-> Coroutine<>", "-> Coroutine<$0>", Some(InsertTextFormat::SNIPPET))
            };
            CompletionItem {
                label: label.to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                insert_text: Some(insert.to_string()),
                insert_text_format: fmt,
                sort_text,
                ..Default::default()
            }
        }
        CandShape::Reach { final_ty, is_method, takes_args } => {
            let key = cand.name.as_str();
            let (label, kind, insert_text, insert_text_format) = if is_method {
                let (insert, fmt) = if strip_call {
                    (key.to_string(), None)
                } else if takes_args {
                    (format!("{key}($0)"), Some(InsertTextFormat::SNIPPET))
                } else {
                    (format!("{key}()"), None)
                };
                (format!("{key}() -> {final_ty}"), CompletionItemKind::METHOD, insert, fmt)
            } else {
                (format!("{key}: {final_ty}"), CompletionItemKind::FIELD, key.to_string(), None)
            };
            CompletionItem {
                label,
                kind: Some(kind),
                detail: Some(final_ty),
                insert_text: Some(insert_text),
                insert_text_format,
                sort_text,
                ..Default::default()
            }
        }
        CandShape::CallThrough { fname, arg: a, payload, multiparam } => {
            // Insert: a tuple/record variant arg carries a payload hole; the
            // trailing param (when multiparam) takes `$0`, so the payload
            // uses `$1` to keep the tab order arg-then-next-param.
            let (insert_text, is_snippet) = match (payload, multiparam) {
                (ArgPayload::None, false) => (format!("{fname}({a})"), false),
                (ArgPayload::None, true) => (format!("{fname}({a}, $0)"), true),
                (ArgPayload::Tuple, false) => (format!("{fname}({a}($0))"), true),
                (ArgPayload::Tuple, true) => (format!("{fname}({a}($1), $0)"), true),
                (ArgPayload::Record, false) => (format!("{fname}({a} {{ $0 }})"), true),
                (ArgPayload::Record, true) => (format!("{fname}({a} {{ $1 }}, $0)"), true),
            };
            let insert_text_format = is_snippet.then_some(InsertTextFormat::SNIPPET);
            // Label shows the payload shape so a tuple/record variant reads
            // as a call to fill, not a bare value.
            let label = match payload {
                ArgPayload::None => format!("{fname}({a})"),
                ArgPayload::Tuple => format!("{fname}({a}(...))"),
                ArgPayload::Record => format!("{fname}({a} {{ ... }})"),
            };
            CompletionItem {
                label,
                kind: Some(CompletionItemKind::FUNCTION),
                insert_text: Some(insert_text),
                insert_text_format,
                sort_text,
                ..Default::default()
            }
        }
        CandShape::Decorator { detail } => CompletionItem {
            label: cand.name.as_str().to_string(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail,
            insert_text: Some(cand.name.as_str().to_string()),
            sort_text,
            ..Default::default()
        },
    }
}

/// The contextual lifecycle-hook keywords with a statement block form
/// (`on_killed { .. }`), parsed specially (`skull-parser` `LifecycleHook`).
pub(crate) fn is_lifecycle_hook(name: &str) -> bool {
    matches!(name, "on_killed" | "on_resume" | "on_hot_reload")
}

/// Completion-parseable variants of the source up to `offset`: keep the prefix
/// (where the receiver and earlier decls live), drop the broken tail, insert a
/// synthetic identifier so the trigger `.` has a field, then close every open
/// `([{`. This is the standard "parse up to the cursor" fixup -- structural
/// resolution needs only a parseable prefix, not a correct whole file.
///
/// Two candidates are tried in order: the bare prefix, then the prefix with an
/// empty block appended -- the latter recovers a dangling block header
/// (`if x.`, `while x.`, `for i in x.`) whose body the user hasn't typed.
///
/// Caveat: delimiter counting ignores string/comment contents, and declarations
/// *after* the cursor are dropped -- acceptable for completion, which edits in
/// the middle of an otherwise-defined file.
/// Inline `labelDetails.detail` for a completion item, rendered directly after
/// the label in the popup: a function shows `(params) -> ret` (the signature
/// minus the redundant `fn name`); a binding/global shows `: type`. None when
/// there's no detail. Structs/enums (label is the bare name, kind conveys it)
/// and fields (label already `name: type`) pass `None`.
pub(crate) fn fn_or_type_label_details(kind: SymCat, detail: Option<&str>) -> Option<CompletionItemLabelDetails> {
    let d = detail?;
    let inline = match kind {
        // `fn name(a: T) -> R` -> `(a: T) -> R`
        SymCat::Function => d.find('(').map_or_else(|| d.to_string(), |i| d[i..].to_string()),
        SymCat::Local | SymCat::Param | SymCat::Global => format!(": {d}"),
        _ => return None,
    };
    Some(CompletionItemLabelDetails { detail: Some(inline), description: None })
}

pub(crate) fn completion_kind(k: SymCat) -> CompletionItemKind {
    match k {
        SymCat::Function => CompletionItemKind::FUNCTION,
        SymCat::Struct => CompletionItemKind::STRUCT,
        SymCat::Enum => CompletionItemKind::ENUM,
        SymCat::Variant => CompletionItemKind::ENUM_MEMBER,
        SymCat::Newtype => CompletionItemKind::CLASS,
        SymCat::Global => CompletionItemKind::VARIABLE,
        SymCat::Local | SymCat::Param => CompletionItemKind::VARIABLE,
        SymCat::Field => CompletionItemKind::FIELD,
        SymCat::Keyword => CompletionItemKind::KEYWORD,
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_max_completions, DEFAULT_MAX_COMPLETION_ITEMS};

    #[test]
    fn max_completions_override_policy() {
        // A positive integer overrides.
        assert_eq!(parse_max_completions(Some("25")), 25);
        assert_eq!(parse_max_completions(Some("  10 ")), 10);
        // Everything else falls back to the default.
        assert_eq!(parse_max_completions(None), DEFAULT_MAX_COMPLETION_ITEMS);
        assert_eq!(parse_max_completions(Some("")), DEFAULT_MAX_COMPLETION_ITEMS);
        assert_eq!(parse_max_completions(Some("lots")), DEFAULT_MAX_COMPLETION_ITEMS);
        assert_eq!(parse_max_completions(Some("0")), DEFAULT_MAX_COMPLETION_ITEMS);
        assert_eq!(parse_max_completions(Some("-5")), DEFAULT_MAX_COMPLETION_ITEMS);
    }
}
