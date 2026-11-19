//! Cursor classification for completion.

use bone_semantic::prelude::{InferenceResult, ModuleId, Workspace};

use crate::backend::{ExpectedArg, expected_for_anchor, fn_unique_return_ty_in_scope};
use crate::cursor_context::{CursorAst, Record};

/// A leading `.` before the identifier under the cursor (a key-path `.field`).
pub(crate) fn leading_dot(src: &str, offset: usize) -> bool {
    let b = src.as_bytes();
    let mut i = offset.min(b.len());
    while i > 0 && is_ident_byte(b[i - 1]) {
        i -= 1;
    }
    i > 0 && b[i - 1] == b'.'
}

pub(crate) fn co_fn_return_slot(src: &[u8], offset: usize) -> bool {
    let mut i = offset.min(src.len());
    while i > 0 && src[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
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
    let mut p = k;
    let skip_ws = |p: &mut usize| {
        while *p > 0 && src[*p - 1].is_ascii_whitespace() {
            *p -= 1;
        }
    };
    skip_ws(&mut p);
    let name_end = p;
    while p > 0 && is_ident_byte(src[p - 1]) {
        p -= 1;
    }
    if p == name_end {
        return false; // no fn name
    }
    skip_ws(&mut p);
    let fn_end = p;
    while p > 0 && is_ident_byte(src[p - 1]) {
        p -= 1;
    }
    if &src[p..fn_end] != b"fn" {
        return false;
    }
    skip_ws(&mut p);
    let co_end = p;
    while p > 0 && is_ident_byte(src[p - 1]) {
        p -= 1;
    }
    &src[p..co_end] == b"co" && (p == 0 || !is_ident_byte(src[p - 1]))
}

pub(crate) fn is_entity_param_name(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "e" | "ent" | "entity")
}

/// Rock identifiers are ASCII, so walking bytes never splits a codepoint.
pub(crate) fn ident_prefix(src: &str, offset: usize) -> &str {
    let end = offset.min(src.len());
    let bytes = src.as_bytes();
    let mut start = end;
    while start > 0 {
        let c = bytes[start - 1];
        if c == b'_' || c.is_ascii_alphanumeric() {
            start -= 1;
        } else {
            break;
        }
    }
    &src[start..end]
}

pub(crate) fn is_ident_byte(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

pub(crate) fn decorator_context(src: &str, offset: usize) -> bool {
    let b = src.as_bytes();
    let mut i = offset.min(b.len());
    while i > 0 && is_ident_byte(b[i - 1]) {
        i -= 1;
    }
    i > 0 && b[i - 1] == b'@'
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

pub(crate) fn in_pattern_position(src: &str, offset: usize) -> bool {
    let b = src.as_bytes();
    let mut i = offset.min(b.len());
    while i < b.len() && is_ident_byte(b[i]) {
        i += 1;
    }
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < b.len() && (b[i] == b'(' || b[i] == b'{') {
        let (open, close) = if b[i] == b'(' {
            (b'(', b')')
        } else {
            (b'{', b'}')
        };
        let mut depth = 0i32;
        while i < b.len() {
            if b[i] == open {
                depth += 1;
            } else if b[i] == close {
                depth -= 1;
                if depth == 0 {
                    i += 1;
                    break;
                }
            }
            i += 1;
        }
    }
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i + 1 < b.len() && b[i] == b'=' && b[i + 1] == b'>' {
        return true;
    }
    match_arm_scrutinee(src, offset).is_some()
}

/// The scrutinee's last byte when the cursor is in a match-arm pattern head.
pub(crate) fn match_arm_scrutinee(src: &str, offset: usize) -> Option<usize> {
    let b = src.as_bytes();
    let mut start = offset.min(b.len());
    while start > 0 && (is_ident_byte(b[start - 1]) || b[start - 1] == b':') {
        start -= 1;
    }
    // Only arm separators or the match opening brace precede a pattern head.
    let mut p = start;
    while p > 0 && b[p - 1].is_ascii_whitespace() {
        p -= 1;
    }
    if p == 0 || !matches!(b[p - 1], b'{' | b',' | b'}') {
        return None;
    }
    match_scrutinee_end(b, start)
}

pub(crate) fn match_scrutinee_end(b: &[u8], from: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = from;
    let brace = loop {
        if i == 0 {
            return None;
        }
        i -= 1;
        match b[i] {
            b')' | b']' | b'}' => depth += 1,
            b'(' | b'[' => {
                if depth == 0 {
                    return None; // inside a call / index group, not a block
                }
                depth -= 1;
            }
            b'{' => {
                if depth == 0 {
                    break i;
                }
                depth -= 1;
            }
            _ => {}
        }
    };
    let mut end = brace;
    while end > 0 && b[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    let mut j = brace;
    let mut depth = 0i32;
    while j > 0 {
        j -= 1;
        match b[j] {
            b')' | b']' => depth += 1,
            b'(' | b'[' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
            }
            b'{' | b'}' | b';' if depth == 0 => return None,
            c if depth == 0 && is_ident_byte(c) => {
                let word_end = j + 1;
                while j > 0 && is_ident_byte(b[j - 1]) {
                    j -= 1;
                }
                if &b[j..word_end] == b"match" {
                    return Some(end - 1);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn for_in_iterable_slot(src: &str, offset: usize) -> bool {
    let b = src.as_bytes();
    let mut p = offset.min(b.len());
    while p > 0 && is_ident_byte(b[p - 1]) {
        p -= 1;
    }
    let skip_ws = |p: &mut usize| {
        while *p > 0 && b[*p - 1].is_ascii_whitespace() {
            *p -= 1;
        }
    };
    skip_ws(&mut p);
    let in_end = p;
    while p > 0 && is_ident_byte(b[p - 1]) {
        p -= 1;
    }
    if &b[p..in_end] != b"in" {
        return false;
    }
    skip_ws(&mut p);
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
        while p > 0 && is_ident_byte(b[p - 1]) {
            p -= 1;
        }
        if p == e {
            return false;
        }
    }
    skip_ws(&mut p);
    let for_end = p;
    while p > 0 && is_ident_byte(b[p - 1]) {
        p -= 1;
    }
    &b[p..for_end] == b"for"
}

pub(crate) enum Context<'a> {
    Member {
        receiver: usize,
        path: Option<&'a [String]>,
    },
    FieldRef {
        callee: &'a str,
        index: usize,
        receiver: Option<usize>,
    },
    Record(&'a Record),
    ModulePath,
    Enum(&'a str),
    Decorator,
}

pub(crate) struct CompletionCx {
    /// None only on the tokenize frontier, where byte-scanner fallbacks apply.
    pub(crate) ast: Option<crate::cursor_context::CursorAst>,
    /// Fallback receiver resolver, used when `type_at` at `member_recv_end` cannot
    /// type the receiver.
    pub(crate) member: Option<Vec<String>>,
    pub(crate) member_recv_end: Option<usize>,
    pub(crate) arg_call: Option<(String, u32, Option<usize>)>,
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
    pub(crate) named_arg: Option<crate::cursor_context::NamedArgSlot>,
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
        cx.and_then(|c| c.call_arg())
    };
    let expr_position = !cx.is_some_and(|c| c.is_type_position());
    CompletionCx {
        member: cx.and_then(|c| c.member_path()),
        member_recv_end: cx.and_then(|c| c.member_receiver_end()),
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
        named_arg: if has_field_value {
            None
        } else {
            cx.and_then(|c| c.named_arg_slot())
        },
        coroutine_generic: cx.is_some_and(|c| c.coroutine_first_generic_arg()),
        pattern_scrutinee: match_arm_scrutinee(src, offset),
        for_in: for_in_iterable_slot(src, offset),
        arg_call,
        expr_position,
        ast,
    }
}

impl CompletionCx {
    pub(crate) fn contexts(&self) -> impl Iterator<Item = Context<'_>> {
        [
            self.member_recv_end.map(|receiver| Context::Member {
                receiver,
                path: self.member.as_deref(),
            }),
            self.arg_call
                .as_ref()
                .map(|(callee, index, receiver)| Context::FieldRef {
                    callee,
                    index: *index as usize,
                    receiver: *receiver,
                }),
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
        if let Some((callee, idx, _recv)) = self.arg_call.as_ref() {
            let recv_ty = self.ast.as_ref().and_then(|cx| {
                cx.call_receiver().and_then(|a| match a {
                    crate::cursor_context::ReceiverAnchor::ChainCall(name) => {
                        let frag = crate::modules_scope::fragment(ws, ws.module(mid).file)?;
                        fn_unique_return_ty_in_scope(ws, &name, frag)
                    }
                    crate::cursor_context::ReceiverAnchor::End(e) => e
                        .checked_sub(1)
                        .and_then(|last| bone_semantic::query::type_at(ws, mid, inf, last))
                        .map(|ta| ta.ty),
                })
            });
            return ExpectedArg::for_call(ws, mid, inf, offset, callee, *idx as usize, recv_ty);
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
