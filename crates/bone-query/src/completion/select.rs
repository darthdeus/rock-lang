use bone_semantic::prelude::Workspace;

use lsp_types::{CompletionItem, CompletionItemKind, CompletionItemLabelDetails, InsertTextFormat};

use crate::query::{CompletionSymbol, SymCat};
use crate::rank_key::RankKey;

use super::rank::ci_cmp;
use crate::query::VariantShape;

pub(crate) fn strip_call_template(items: &mut [CompletionItem]) {
    for it in items.iter_mut() {
        let Some(t) = it.insert_text.as_deref() else {
            continue;
        };
        if let Some(bare) = t.strip_suffix("($0)").or_else(|| t.strip_suffix("()")) {
            it.insert_text = Some(bare.to_string());
            it.insert_text_format = None;
        }
    }
}

pub(crate) fn member_tail(src: &str, offset: usize) -> &str {
    let rest = src.get(offset..).unwrap_or("");
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '(' | ')' | '?' | '!')))
        .unwrap_or(rest.len());
    &rest[..end]
}

/// Snippets are excluded because their placeholders are not literal text.
pub(crate) fn reconcile_suffix(items: &mut [CompletionItem], tail: &str) {
    if !tail.starts_with(['.', '(']) {
        return;
    }
    for it in items.iter_mut() {
        if matches!(it.insert_text_format, Some(InsertTextFormat::SNIPPET)) {
            continue;
        }
        let Some(t) = it.insert_text.as_deref() else {
            continue;
        };
        let mut k = tail.len();
        while k > 0 {
            if tail.is_char_boundary(k) && t.ends_with(&tail[..k]) {
                it.insert_text = Some(t[..t.len() - k].to_string());
                break;
            }
            k -= 1;
        }
    }
}

/// Cap general results because incomplete lists are re-requested per keystroke.
pub(crate) const MAX_COMPLETION_ITEMS: usize = 200;

pub(crate) const REACH_CAP: usize = 64;

pub(crate) const FREQ_HOT: u32 = 16;
pub(crate) const FREQ_WARM: u32 = 3;

pub(crate) const CALL_THROUGH_CAP: usize = 10;
pub(crate) const PER_FN_METHOD_CAP: usize = 2;

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

pub(crate) enum CandShape {
    Entry(crate::query::CompletionEntry),
    EntitySynth {
        payload: Option<ustr::Ustr>,
    },
    NamedArg {
        pname: ustr::Ustr,
        pty: String,
    },
    Wait,
    Coroutine {
        with_wait: bool,
    },
    CoReturn {
        has_wait: bool,
    },
    /// A reachability path; the cand name is the access form `root.suffix`.
    Reach {
        final_ty: String,
        is_method: bool,
        takes_args: bool,
    },
    /// A synthesized whole call; the cand name is `fname(arg)`.
    CallThrough {
        fname: ustr::Ustr,
        arg: String,
        payload: VariantShape,
        multiparam: bool,
    },
    Decorator {
        detail: Option<String>,
    },
    FieldRef {
        ty: String,
        add_dot: bool,
    },
    Method {
        signature: String,
        takes_args: bool,
        snippet: Option<String>,
    },
    Variant {
        shape: crate::query::VariantShape,
        filter_text: Option<String>,
    },
    Label {
        kind: CompletionItemKind,
    },
    EntityUnwrap,
}

/// Clamp rank fields so comparison and the encoded sort text agree.
pub(crate) struct Cand {
    pub(crate) key: RankKey,
    pub(crate) name: CandName,
    pub(crate) shape: CandShape,
}

impl Cand {
    pub(crate) fn new(key: RankKey, name: CandName, shape: CandShape) -> Self {
        Cand {
            key: key.clamped(),
            name,
            shape,
        }
    }
}

pub(crate) struct Sink {
    pub(crate) cands: Vec<Cand>,
    pub(crate) seen: std::collections::HashSet<String>,
}

impl Sink {
    pub(crate) fn new() -> Self {
        Sink {
            cands: Vec::new(),
            seen: std::collections::HashSet::new(),
        }
    }

    pub(crate) fn push(&mut self, cand: Cand) {
        self.cands.push(cand);
    }

    pub(crate) fn admit(&mut self, name: &str) -> bool {
        if self.seen.contains(name) {
            return false;
        }
        self.seen.insert(name.to_string());
        true
    }

    pub(crate) fn retain_not_named(&mut self, name: &str) {
        self.cands.retain(|c| c.name.as_str() != name);
    }

    pub(crate) fn len(&self) -> usize {
        self.cands.len()
    }
}

pub(crate) fn cand_cmp(a: &Cand, b: &Cand) -> std::cmp::Ordering {
    a.key
        .cmp(&b.key)
        .then_with(|| ci_cmp(a.name.as_str().as_bytes(), b.name.as_str().as_bytes()))
}

pub(crate) fn materialize_cand(
    cand: Cand,
    ws: &Workspace,
    fragment_file: ustr::Ustr,
    strip_call: bool,
    pattern: bool,
) -> CompletionItem {
    let sort_text = Some(cand.key.encode(cand.name.as_str()));
    match cand.shape {
        CandShape::Entry(c) => {
            let kind = c.kind();
            let name = bone_core::modules::display_name(c.name.as_str());
            let detail = match c.symbol {
                // An alias can share its display spelling with another definition.
                CompletionSymbol::Function { key, .. } => c.detail.or_else(|| {
                    crate::query::render_deferred_signature(ws, fragment_file, key, c.name)
                }),
                CompletionSymbol::Variable {
                    kind: SymCat::Global,
                    ty,
                    ..
                } => c.detail.or_else(|| {
                    ty.and_then(|ty| {
                        let rendered = ws.render(None, ty);
                        (!rendered.is_empty()).then_some(rendered)
                    })
                }),
                _ => c.detail,
            };
            let label_details = fn_or_type_label_details(kind, detail.as_deref());
            let (insert_text, insert_text_format) = match c.symbol {
                CompletionSymbol::Function { .. } if is_lifecycle_hook(c.name.as_str()) => (
                    Some(format!("{name} {{ $0 }}")),
                    Some(InsertTextFormat::SNIPPET),
                ),
                CompletionSymbol::Function { takes_params, .. }
                | CompletionSymbol::Macro { takes_params } => {
                    let (text, format) = call_insert(name, strip_call, takes_params);
                    (Some(text), format)
                }
                CompletionSymbol::Variant { shape, .. } => {
                    variant_insert(name, shape, pattern, strip_call)
                }
                _ => (None, None),
            };
            CompletionItem {
                label: name.to_string(),
                kind: Some(completion_kind(kind)),
                label_details,
                detail,
                insert_text,
                insert_text_format,
                sort_text,
                ..Default::default()
            }
        }
        CandShape::EntitySynth { payload } => type_template(
            "Entity",
            payload.as_ref().map(|name| name.as_str()),
            CompletionItemKind::STRUCT,
            sort_text,
        ),
        CandShape::NamedArg { pname, pty } => CompletionItem {
            label: format!("{pname}:"),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(pty),
            insert_text: Some(format!("{pname}: ")),
            sort_text,
            ..Default::default()
        },
        CandShape::Wait => CompletionItem {
            label: "Wait".to_string(),
            kind: Some(CompletionItemKind::STRUCT),
            sort_text,
            ..Default::default()
        },
        CandShape::Coroutine { with_wait } => type_template(
            "Coroutine",
            with_wait.then_some("Wait"),
            CompletionItemKind::STRUCT,
            sort_text,
        ),
        CandShape::CoReturn { has_wait } => type_template(
            "-> Coroutine",
            has_wait.then_some("Wait"),
            CompletionItemKind::SNIPPET,
            sort_text,
        ),
        CandShape::Reach {
            final_ty,
            is_method,
            takes_args,
        } => {
            let key = cand.name.as_str();
            let (label, kind, insert_text, insert_text_format) = if is_method {
                let (insert, fmt) = call_insert(key, strip_call, takes_args);
                (
                    format!("{key}() -> {final_ty}"),
                    CompletionItemKind::METHOD,
                    insert,
                    fmt,
                )
            } else {
                (
                    format!("{key}: {final_ty}"),
                    CompletionItemKind::FIELD,
                    key.to_string(),
                    None,
                )
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
        CandShape::CallThrough {
            fname,
            arg: a,
            payload,
            multiparam,
        } => {
            // The payload uses `$1` so tab order is arg, then the trailing param's `$0`.
            let (insert_text, is_snippet) = match (payload, multiparam) {
                (VariantShape::Unit, false) => (format!("{fname}({a})"), false),
                (VariantShape::Unit, true) => (format!("{fname}({a}, $0)"), true),
                (VariantShape::Tuple, false) => (format!("{fname}({a}($0))"), true),
                (VariantShape::Tuple, true) => (format!("{fname}({a}($1), $0)"), true),
                (VariantShape::Record, false) => (format!("{fname}({a} {{ $0 }})"), true),
                (VariantShape::Record, true) => (format!("{fname}({a} {{ $1 }}, $0)"), true),
            };
            let insert_text_format = is_snippet.then_some(InsertTextFormat::SNIPPET);
            let label = match payload {
                VariantShape::Unit => format!("{fname}({a})"),
                VariantShape::Tuple => format!("{fname}({a}(...))"),
                VariantShape::Record => format!("{fname}({a} {{ ... }})"),
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
        CandShape::FieldRef { ty, add_dot } => {
            let name = cand.name.as_str();
            CompletionItem {
                label: format!(".{name}: {ty}"),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(ty),
                sort_text,
                filter_text: Some(name.to_string()),
                insert_text: Some(if add_dot {
                    format!(".{name}")
                } else {
                    name.to_string()
                }),
                ..Default::default()
            }
        }
        CandShape::Method {
            signature,
            takes_args,
            snippet,
        } => {
            let name = cand.name.as_str();
            let (insert, format) = match snippet {
                Some(s) if !strip_call => (s, Some(InsertTextFormat::SNIPPET)),
                _ => call_insert(name, strip_call, takes_args),
            };
            CompletionItem {
                label: name.to_string(),
                label_details: fn_or_type_label_details(SymCat::Function, Some(&signature)),
                kind: Some(CompletionItemKind::METHOD),
                detail: Some(signature),
                insert_text: Some(insert),
                insert_text_format: format,
                sort_text,
                ..Default::default()
            }
        }
        CandShape::Variant { shape, filter_text } => {
            let name = cand.name.as_str();
            let (insert_text, insert_text_format) =
                variant_insert(name, shape, pattern, strip_call);
            CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                filter_text,
                insert_text,
                insert_text_format,
                sort_text,
                ..Default::default()
            }
        }
        CandShape::Label { kind } => CompletionItem {
            label: cand.name.as_str().to_string(),
            kind: Some(kind),
            sort_text,
            ..Default::default()
        },
        CandShape::EntityUnwrap => CompletionItem {
            label: cand.name.as_str().to_string(),
            kind: Some(CompletionItemKind::METHOD),
            detail: Some("unwrap the Entity's value (T)".to_string()),
            insert_text: Some(cand.name.as_str().to_string()),
            sort_text,
            ..Default::default()
        },
    }
}

fn call_insert(name: &str, strip: bool, takes_args: bool) -> (String, Option<InsertTextFormat>) {
    if strip {
        (name.to_string(), None)
    } else if takes_args {
        (format!("{name}($0)"), Some(InsertTextFormat::SNIPPET))
    } else {
        (format!("{name}()"), None)
    }
}

fn type_template(
    ctor: &str,
    payload: Option<&str>,
    kind: CompletionItemKind,
    sort_text: Option<String>,
) -> CompletionItem {
    CompletionItem {
        label: format!("{ctor}<{}>", payload.unwrap_or("")),
        kind: Some(kind),
        insert_text: Some(format!("{ctor}<{}>", payload.unwrap_or("$0"))),
        insert_text_format: payload.is_none().then_some(InsertTextFormat::SNIPPET),
        sort_text,
        ..Default::default()
    }
}

pub(crate) fn variant_insert(
    name: &str,
    shape: crate::query::VariantShape,
    pattern: bool,
    strip_call: bool,
) -> (Option<String>, Option<InsertTextFormat>) {
    use crate::query::VariantShape;
    match (pattern, shape) {
        (false, VariantShape::Tuple) if strip_call => (Some(name.to_string()), None),
        (false, VariantShape::Tuple) => {
            (Some(format!("{name}($0)")), Some(InsertTextFormat::SNIPPET))
        }
        (false, VariantShape::Record) => (
            Some(format!("{name} {{ $0 }}")),
            Some(InsertTextFormat::SNIPPET),
        ),
        _ => (None, None),
    }
}

/// Parsed specially as `LifecycleHook` by `skull-parser`.
pub(crate) fn is_lifecycle_hook(name: &str) -> bool {
    matches!(name, "on_killed" | "on_resume" | "on_hot_reload")
}

pub(crate) fn fn_or_type_label_details(
    kind: SymCat,
    detail: Option<&str>,
) -> Option<CompletionItemLabelDetails> {
    let d = detail?;
    let inline = match kind {
        // `fn name(a: T) -> R` -> `(a: T) -> R`
        SymCat::Function | SymCat::Macro => d
            .find('(')
            .map_or_else(|| d.to_string(), |i| d[i..].to_string()),
        SymCat::Local | SymCat::Param | SymCat::Global => format!(": {d}"),
        _ => return None,
    };
    Some(CompletionItemLabelDetails {
        detail: Some(inline),
        description: None,
    })
}

pub(crate) fn completion_kind(k: SymCat) -> CompletionItemKind {
    match k {
        // LSP has no macro kind; a macro is invoked like a call, so FUNCTION
        // is the honest render.
        SymCat::Function | SymCat::Macro => CompletionItemKind::FUNCTION,
        SymCat::Struct => CompletionItemKind::STRUCT,
        SymCat::Enum => CompletionItemKind::ENUM,
        SymCat::Variant => CompletionItemKind::ENUM_MEMBER,
        SymCat::Newtype => CompletionItemKind::CLASS,
        SymCat::Global => CompletionItemKind::VARIABLE,
        SymCat::Local | SymCat::Param => CompletionItemKind::VARIABLE,
        SymCat::Keyword => CompletionItemKind::KEYWORD,
    }
}
