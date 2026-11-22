//! Cursor classification for completion.

use bone_semantic::prelude::{InferenceResult, ModuleId, Workspace};

use crate::backend::{ExpectedArg, expected_for_anchor, fn_unique_return_ty_in_scope};
use crate::cursor_context::{CallArgument, CursorAst, Record};

/// A leading `.` before the identifier under the cursor (a key-path `.field`).
pub(crate) fn leading_dot(src: &str, offset: usize) -> bool {
    byte_before_ident(src, offset) == Some(b'.')
}

fn byte_before_ident(src: &str, offset: usize) -> Option<u8> {
    let b = src.as_bytes();
    let start = ident_start(b, offset.min(b.len()));
    start.checked_sub(1).map(|i| b[i])
}

fn skip_back(b: &[u8], mut end: usize, skip: impl Fn(u8) -> bool) -> usize {
    while end > 0 && skip(b[end - 1]) {
        end -= 1;
    }
    end
}

pub(crate) fn ident_start(b: &[u8], end: usize) -> usize {
    skip_back(b, end, is_ident_byte)
}

fn ws_start(b: &[u8], end: usize) -> usize {
    skip_back(b, end, |c| c.is_ascii_whitespace())
}

/// The identifier ending at `end` after skipping whitespace before it.
fn word_before(b: &[u8], end: usize) -> (usize, &[u8]) {
    let word_end = ws_start(b, end);
    let start = ident_start(b, word_end);
    (start, &b[start..word_end])
}

pub(crate) fn co_fn_return_slot(src: &[u8], offset: usize) -> bool {
    let i = ws_start(src, offset.min(src.len()));
    if i == 0 || src[i - 1] != b')' {
        return false;
    }
    let mut depth = 0i32;
    let mut k = i - 1;
    loop {
        match src[k] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        if k == 0 {
            return false;
        }
        k -= 1;
    }
    let (name_start, name) = word_before(src, k);
    if name.is_empty() {
        return false;
    }
    let (fn_start, fn_kw) = word_before(src, name_start);
    fn_kw == b"fn" && word_before(src, fn_start).1 == b"co"
}

pub(crate) fn is_entity_param_name(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "e" | "ent" | "entity")
}

/// Rock identifiers are ASCII, so walking bytes never splits a codepoint.
pub(crate) fn ident_prefix(src: &str, offset: usize) -> &str {
    let end = offset.min(src.len());
    &src[ident_start(src.as_bytes(), end)..end]
}

/// The identifier byte range that `offset` touches, empty between non-identifiers.
pub(crate) fn ident_range_around(src: &str, offset: usize) -> (usize, usize) {
    let b = src.as_bytes();
    let at = offset.min(b.len());
    let end = at + b[at..].iter().take_while(|&&c| is_ident_byte(c)).count();
    (ident_start(b, at), end)
}

pub(crate) fn is_ident_byte(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

pub(crate) fn decorator_context(src: &str, offset: usize) -> bool {
    byte_before_ident(src, offset) == Some(b'@')
}

pub(crate) fn completion_munges(src: &str, offset: usize) -> [String; 2] {
    let off = offset.min(src.len());
    let (head, tail) = src.split_at(off);

    // A probe makes incomplete positions still produce an AST node.
    let mk = |inject: &str| {
        let mut s = String::with_capacity(src.len() + 32);
        s.push_str(head);
        s.push_str("__rock_completion__");
        s.push_str(inject);
        s.push_str(tail);
        balance_delimiters(s)
    };

    [mk(""), mk(" {}")]
}

pub(crate) fn balance_delimiters(mut s: String) -> String {
    // An unterminated `"foo {` must close as `}"`, not `"`, or the tokenizer
    // still fatals on the unmatched interpolation.
    #[derive(PartialEq)]
    enum Ctx {
        Paren,
        Bracket,
        Brace,
        Interp,
        LineStr,
        TripleStr,
    }
    let mut stack: Vec<Ctx> = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_line_comment = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        match stack.last() {
            Some(Ctx::LineStr) | Some(Ctx::TripleStr) => {
                let triple = stack.last() == Some(&Ctx::TripleStr);
                match b {
                    b'\\' => i += 1, // skip the escaped character
                    b'"' if triple && bytes.get(i + 1..i + 3) == Some(b"\"\"") => {
                        stack.pop();
                        i += 2;
                    }
                    b'"' if !triple => {
                        stack.pop();
                    }
                    // `{{` emits a literal `{`
                    b'{' if bytes.get(i + 1) == Some(&b'{') => i += 1,
                    b'{' => stack.push(Ctx::Interp),
                    _ => {}
                }
                i += 1;
            }
            _ => {
                match b {
                    b'/' if bytes.get(i + 1) == Some(&b'/') => {
                        in_line_comment = true;
                        i += 1;
                    }
                    b'"' if bytes.get(i + 1..i + 3) == Some(b"\"\"") => {
                        stack.push(Ctx::TripleStr);
                        i += 2;
                    }
                    b'"' => stack.push(Ctx::LineStr),
                    b'(' => stack.push(Ctx::Paren),
                    b'[' => stack.push(Ctx::Bracket),
                    b'{' => stack.push(Ctx::Brace),
                    b')' | b']' | b'}' => {
                        stack.pop();
                    }
                    _ => {}
                }
                i += 1;
            }
        }
    }
    if in_line_comment && !stack.is_empty() {
        s.push('\n'); // source ends inside a `//` comment; keep closers out of it
    }
    while let Some(c) = stack.pop() {
        s.push_str(match c {
            Ctx::Paren => ")",
            Ctx::Bracket => "]",
            Ctx::Brace => "}",
            Ctx::Interp => "}",
            Ctx::LineStr => "\"",
            Ctx::TripleStr => "\"\"\"",
        });
    }
    s
}

pub(crate) fn for_in_iterable_slot(src: &str, offset: usize) -> bool {
    let b = src.as_bytes();
    let (in_start, in_kw) = word_before(b, ident_start(b, offset.min(b.len())));
    if in_kw != b"in" {
        return false;
    }
    let mut p = ws_start(b, in_start);
    if p > 0 && b[p - 1] == b')' {
        let mut depth = 0i32;
        loop {
            if p == 0 {
                return false;
            }
            p -= 1;
            match b[p] {
                b')' => depth += 1,
                b'(' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
    } else {
        let e = p;
        p = ident_start(b, p);
        if p == e {
            return false;
        }
    }
    word_before(b, p).1 == b"for"
}

pub(crate) enum Context<'a> {
    Member {
        receiver: usize,
        path: Option<&'a [String]>,
    },
    FieldRef(&'a CallArgument),
    Record(&'a Record),
    ModulePath,
    Enum(&'a str),
    Decorator,
}

pub(crate) struct CompletionCx {
    /// None only on the tokenize frontier, where byte-scanner fallbacks apply.
    pub(crate) ast: Option<crate::cursor_context::CursorAst>,
    /// The receiver's end offset and optional path for lookup when inference fails.
    member: Option<(usize, Option<Vec<String>>)>,
    pub(crate) arg_call: Option<CallArgument>,
    record: Option<Record>,
    qualified: Option<String>,
    pub(crate) decorator: bool,
    /// Only when not in a call arg, which is the more specific expected-type source.
    pub(crate) field_value: Option<(String, String, bool)>,
    pub(crate) anchor: Option<SlotAnchor>,
    pub(crate) bool_chain: bool,
    pub(crate) expr_position: bool,
    pub(crate) after_yield: bool,
    pub(crate) param_type: Option<(String, String)>,
    pub(crate) coroutine_generic: bool,
    pub(crate) pattern_scrutinee: Option<usize>,
    pub(crate) for_in: bool,
}

pub(crate) fn classify(src: &str, offset: usize) -> CompletionCx {
    let ast = crate::cursor_context::CursorAst::at(src, offset);
    let cx = ast.as_ref();
    let (record, field_value) = cx.map_or((None, None), CursorAst::record_contexts);
    // An inner call slot can take precedence over the enclosing field value.
    let has_field_value = field_value.is_some();
    let arg_call = if has_field_value {
        None
    } else {
        cx.and_then(CursorAst::call_argument)
    };
    let expr_position = !cx.is_some_and(|c| c.is_type_position());
    CompletionCx {
        member: cx.and_then(CursorAst::member_receiver),
        record,
        qualified: cx.and_then(|c| c.qualified_base()),
        decorator: decorator_context(src, offset),
        field_value,
        anchor: cx.and_then(slot_anchor),
        bool_chain: cx.is_some_and(|c| c.bool_condition_chain()),
        after_yield: expr_position && cx.is_some_and(|c| c.after_yield()),
        param_type: if expr_position {
            None
        } else {
            cx.and_then(|c| c.param_type())
        },
        coroutine_generic: cx.is_some_and(|c| c.coroutine_first_generic_arg()),
        pattern_scrutinee: cx.and_then(CursorAst::match_pattern_scrutinee),
        for_in: for_in_iterable_slot(src, offset),
        arg_call,
        expr_position,
        ast,
    }
}

impl CompletionCx {
    pub(crate) fn contexts(&self) -> impl Iterator<Item = Context<'_>> {
        [
            self.member
                .as_ref()
                .map(|(receiver, path)| Context::Member {
                    receiver: *receiver,
                    path: path.as_deref(),
                }),
            self.arg_call.as_ref().map(Context::FieldRef),
            self.record
                .as_ref()
                .filter(|r| matches!(r, Record::Variant { .. }))
                .map(Context::Record),
            Some(Context::ModulePath),
            self.qualified.as_deref().map(Context::Enum),
            self.record
                .as_ref()
                .filter(|r| matches!(r, Record::Struct { .. }))
                .map(Context::Record),
            self.decorator.then_some(Context::Decorator),
        ]
        .into_iter()
        .flatten()
    }

    pub(crate) fn expected<'a>(
        &self,
        ws: &'a Workspace,
        mid: ModuleId,
        inf: &'a InferenceResult,
        offset: usize,
    ) -> Option<ExpectedArg<'a>> {
        if let Some(call) = &self.arg_call {
            let recv_ty = call.receiver.as_ref().and_then(|receiver| {
                if let Some(name) = receiver.return_from {
                    let frag = crate::modules_scope::fragment(ws, ws.module(mid).file)?;
                    return fn_unique_return_ty_in_scope(ws, name.as_str(), frag);
                }
                let end = receiver.end.checked_sub(1)?;
                bone_semantic::query::type_at(ws, mid, inf, end).map(|at| at.ty)
            });
            return ExpectedArg::for_call(ws, mid, inf, offset, &call.callee, call.index, recv_ty);
        }
        if let Some((sname, fname, is_lambda)) = self.field_value.as_ref() {
            // Inside a lambda assigned to a function-typed field the body returns the
            // field's return type. The parser keeps source spelling; workspace type
            // indexes use canonical keys for standalone files.
            let sname =
                crate::modules_scope::resolve_type_key_in_file(ws, ws.module(mid).file, sname)?;
            let slot = if *is_lambda {
                crate::query::struct_field_fn_return_ty(ws, sname.as_str(), fname)
                    .or_else(|| crate::query::struct_field_ty(ws, sname.as_str(), fname))
            } else {
                crate::query::struct_field_ty(ws, sname.as_str(), fname)
            }?;
            return ExpectedArg::for_slot(ws, mid, inf, offset, slot);
        }
        // A bare assign target (`dest = |`) is typed `()` in place position, so the
        // byte anchor yields no type; a nested `==` RHS still wins.
        if let Some(cx) = self.ast.as_ref() {
            if cx.comparison_lhs().is_none() {
                if let Some(name) = cx.assign_target_ident() {
                    if let Some(ty) = crate::query::binding_ty_by_name(ws, mid, inf, offset, &name)
                    {
                        return ExpectedArg::for_slot(ws, mid, inf, offset, ty);
                    }
                }
            }
        }
        self.anchor
            .and_then(|a| expected_for_anchor(ws, mid, inf, offset, a))
    }
}

/// Anchors sit before the cursor so they also work in speculative reparses.
#[derive(Clone, Copy)]
pub(crate) enum SlotAnchor {
    TypeAt(usize),
    EnclosingFnReturn,
}

/// A `==` nested in an assignment RHS is the more specific slot.
pub(crate) fn slot_anchor(cx: &crate::cursor_context::CursorAst) -> Option<SlotAnchor> {
    cx.comparison_lhs()
        .or_else(|| cx.assign_lhs())
        .or_else(|| cx.typed_let_binding())
        .map(SlotAnchor::TypeAt)
        .or_else(|| {
            (cx.in_return_value() || cx.in_tail_return()).then_some(SlotAnchor::EnclosingFnReturn)
        })
}
