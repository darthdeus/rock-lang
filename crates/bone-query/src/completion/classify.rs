//! Cursor classification for completion (completion-v2 stage A): the
//! syntactic byte/AST classifiers and `classify()` -- every classifier
//! consulted exactly once into a `CompletionCx`. Semantic resolution
//! (`ExpectedArg`, Session variants) stays in `backend.rs`.

use bone_semantic::prelude::{InferenceResult, ModuleId, Workspace};

use crate::backend::{expected_for_anchor, fn_unique_return_ty, ExpectedArg};

/// True when a leading `.` sits just before the partial identifier under the
/// cursor (a key-path `.field`, not member access -- `receiver_path` already
/// returned for a receiver-preceded dot). Decides insert text: `co` when the
/// dot is present, `.co` to add it.
pub(crate) fn leading_dot(src: &str, offset: usize) -> bool {
    let b = src.as_bytes();
    let mut i = offset.min(b.len());
    while i > 0 && is_ident_byte(b[i - 1]) {
        i -= 1;
    }
    i > 0 && b[i - 1] == b'.'
}

/// True when `offset` sits just after the `)` of a `co fn NAME(..)` signature
/// with no return type yet (`co fn spawn()|`). Pure byte heuristic; drives the
/// `-> Coroutine<CoFrame>` return-type snippet.
pub(crate) fn co_fn_return_slot(src: &[u8], offset: usize) -> bool {
    let mut i = offset.min(src.len());
    while i > 0 && src[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 || src[i - 1] != b')' {
        return false;
    }
    // balance back to the matching `(`
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
    // before the `(`: ws, fn-name ident, ws, `fn`, ws, `co`
    let mut p = k;
    let skip_ws = |p: &mut usize| while *p > 0 && src[*p - 1].is_ascii_whitespace() { *p -= 1; };
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

/// Param names that conventionally hold an `Entity<T>` (the `e`/`entity` idiom).
pub(crate) fn is_entity_param_name(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "e" | "ent" | "entity")
}

/// The identifier characters immediately preceding `offset` (a byte offset).
/// Rock identifiers are ASCII (`[A-Za-z0-9_]`), so walking bytes never splits a
/// multibyte codepoint -- a non-ASCII byte is not alphanumeric and stops the walk.
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

/// True when the cursor is in decorator position (`@<ident-prefix>|`): the ident
/// under the cursor is immediately preceded by an `@`. The `@` isn't an ident
/// byte, so `ident_prefix` already excludes it -- this only checks the trigger.
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

    // Inject a sentinel identifier at the cursor (rust-analyzer's technique) so
    // the in-progress `recv.` / `Foo {` parses, then KEEP the rest of the file:
    // items defined AFTER the cursor (e.g. a UFCS method later in the same file)
    // must stay in the parse, or they're silently missing from completion.
    // skull-parser recovers at item/statement granularity, so the partially
    // sentinel-mangled current statement drops by itself without taking the tail
    // with it. balance_delimiters runs over head+tail together, so a delimiter
    // the tail already closes adds no spurious closer.
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

/// Append a closer for every still-open `([{` in `s` (innermost first).
///
/// Brace counting skips `//` line comments and `"`-delimited strings, so a brace
/// in trailing prose (`// missing }`) or a string literal isn't mistaken for real
/// nesting. When the completion munge keeps the file tail, the tail often ends in
/// a `// ...` comment with no trailing newline; the appended closers go on a fresh
/// line so they aren't swallowed by that comment (which would leave the enclosing
/// block unclosed and fail the whole parse). An unterminated string is closed too.
pub(crate) fn balance_delimiters(mut s: String) -> String {
    // One stack models the whole nesting: code delimiters, strings, and the
    // interpolation braces that re-enter CODE inside a string (`"a {expr}"`)
    // -- an unterminated `"foo {` must close as `}"`, not `"`, or the
    // tokenizer still fatals on the unmatched interpolation.
    #[derive(PartialEq)]
    enum Ctx {
        Paren,
        Bracket,
        Brace,
        /// `{` inside a string: code until the matching `}`.
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
                    // `{{` emits a literal `{`; a bare `{` opens interpolation
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
                        // a `}` closes a code Brace or pops Interp back into
                        // its string; `)`/`]` close their code delimiter
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

/// True if the `Enum::Variant` being completed at `offset` sits in a match-arm
/// **pattern** position rather than a value position. Two heuristics, in order:
///
/// 1. **Forward `=>` scan** -- for an arm whose body is already present, scan
///    past the variant name and an optional balanced payload group (`(..)` /
///    `{..}`); the next tokens being `=>` mark an arm pattern. A value position
///    (`let x = ..`, `return ..`, a call argument) never leads to `=>`.
/// 2. **Backward enclosing-`match` scan** -- a FRESH arm (`SpellState::Loa|`
///    with no `=>` typed yet) leads to no `=>`, and is lexically indistinguishable
///    from a value position by what precedes it (`{`/`,` open blocks AND args
///    too). The reliable signal is that the nearest enclosing block is a
///    `match <scrutinee> { .. }`, so walk out to that brace and check it.
///
/// Patterns bind, they don't construct -- so in either case no payload template
/// is offered.
pub(crate) fn in_pattern_position(src: &str, offset: usize) -> bool {
    let b = src.as_bytes();
    let mut i = offset.min(b.len());
    // the variant identifier under / after the cursor
    while i < b.len() && is_ident_byte(b[i]) {
        i += 1;
    }
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    // an already-typed payload pattern, e.g. `Loaded(x)` / `Bandage { .. }`
    if i < b.len() && (b[i] == b'(' || b[i] == b'{') {
        let (open, close) = if b[i] == b'(' { (b'(', b')') } else { (b'{', b'}') };
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
    // Fresh arm: no `=>` yet -- the arm-head classification below.
    match_arm_scrutinee(src, offset).is_some()
}

/// When the cursor sits in a match-arm pattern HEAD (fresh arm or an
/// already-`=>`ed one being retyped), the byte anchor of the SCRUTINEE's last
/// byte -- `type_at` there is the scrutinee's type, which names the enum
/// whose variants the pattern slot expects. `None` in every other position.
pub(crate) fn match_arm_scrutinee(src: &str, offset: usize) -> Option<usize> {
    let b = src.as_bytes();
    // Start of the `Enum::Variant` path the cursor is in (ident bytes + `:`
    // run backward).
    let mut start = offset.min(b.len());
    while start > 0 && (is_ident_byte(b[start - 1]) || b[start - 1] == b':') {
        start -= 1;
    }
    // The significant byte just before the path. An arm HEAD is opened by the
    // match `{`, an arm separator `,`, or a previous arm's block-body close `}`.
    // An arm BODY (value position) is preceded by `=>`; `let`/call/index/struct
    // contexts by `=`/`(`/`[`/`:` -- all rejected here. This excludes arm bodies
    // so the enclosing-`match` test below only fires for actual arm heads.
    let mut p = start;
    while p > 0 && b[p - 1].is_ascii_whitespace() {
        p -= 1;
    }
    if p == 0 || !matches!(b[p - 1], b'{' | b',' | b'}') {
        return None;
    }
    match_scrutinee_end(b, start)
}

/// When the innermost block enclosing byte `from` is the body of a `match`
/// expression, the index of the scrutinee's LAST byte (the non-ws byte just
/// before the match `{`); `None` otherwise. Walks backward to the nearest
/// unmatched `{` (balancing `()[]{}`); a `(`/`[` reached first means we are
/// inside a call/index, not a block, so not a match. From that `{`, scans the
/// scrutinee back to the `match` keyword: only `()`/`[]`-balanced scrutinee
/// tokens may intervene; a `{`/`}`/`;` boundary at depth 0 ends the search
/// (the brace governs something else, e.g. a fn body or struct literal).
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
    // Scan the scrutinee backward from the `{` to the governing keyword.
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
                // Read this whole word. `match` -> the brace is a match body.
                // Any other word is part of the scrutinee (`match foo.bar {`) or
                // the head of some other construct (`fn r() {`); keep scanning
                // back -- the `{`/`}`/`;` boundary arms above stop a non-match
                // brace before a spurious earlier `match` can be reached.
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

/// True when the cursor's ident sits in the ITERABLE slot of a `for` loop
/// (`for x in |`, `for (k, v) in |`): preceded by the `in` keyword, a
/// binding pattern (plain ident or `(..)` tuple), and the `for` keyword.
/// Pure byte heuristic, same family as `co_fn_return_slot`.
pub(crate) fn for_in_iterable_slot(src: &str, offset: usize) -> bool {
    let b = src.as_bytes();
    let mut p = offset.min(b.len());
    while p > 0 && is_ident_byte(b[p - 1]) {
        p -= 1;
    }
    let skip_ws = |p: &mut usize| while *p > 0 && b[*p - 1].is_ascii_whitespace() { *p -= 1; };
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
        // `(a, b)` tuple binding: balance back to its `(`
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

/// The cursor's completion classification, computed ONCE per request by
/// [`classify`] -- every context consultation lives there, so a new syntax
/// case is one field + one consultation + one dispatch arm
/// (docs/completion-v2-design.md M2b). Field order below is the dispatch
/// precedence. Classification is SYNTACTIC; handlers may still fall through
/// at runtime (an `X {` head that isn't a struct, a call slot that isn't
/// FieldOf), which is why the dispatcher keeps an ordered attempt chain
/// rather than a single-winner match.
pub(crate) struct CompletionCx {
    /// The recovered-AST classifier over the munge ladder; None only on the
    /// tokenize frontier (byte-scanner fallbacks own that).
    pub(crate) ast: Option<crate::cursor_context::CursorAst>,
    /// `recv.pa|` -- pure ident receiver chain, as name segments. The FALLBACK
    /// receiver resolver (structural name-walk): used when `type_at` at
    /// `member_recv_end` can't type the receiver (unknown callee / dropped
    /// statement), where a by-name binding lookup still resolves it.
    pub(crate) member: Option<Vec<String>>,
    /// `recv.<cursor>` -- the member receiver's end offset, any receiver shape.
    /// The PRIMARY resolver: the receiver's type via inference (`type_at`).
    pub(crate) member_recv_end: Option<usize>,
    /// Enclosing call with the cursor in an ARG slot: (callee, idx,
    /// receiver end). Field-ref gate + expected-type input.
    pub(crate) arg_call: Option<(String, u32, Option<usize>)>,
    /// `Enum::Variant { fi| }` record-literal field names.
    pub(crate) enum_record: Option<(String, String)>,
    /// `Enum::|` / `Struct::|` qualified-path members.
    pub(crate) qualified: Option<String>,
    /// `Name { fi| }` struct-literal field-NAME slot.
    pub(crate) struct_literal: Option<(String, usize)>,
    /// `@deco|` decorator namespace (byte trigger; live even when the
    /// module dropped out).
    pub(crate) decorator: bool,
    /// `Name { field: <cursor> }` field-VALUE slot -- only when NOT in a
    /// call arg (the arg slot is the more specific expected-type source).
    pub(crate) field_value: Option<(String, String, bool)>,
    /// Slot-anchored expected type (assign/compare RHS, typed-`let`,
    /// `return`) -- consumed by BOTH the general and the member path.
    pub(crate) anchor: Option<SlotAnchor>,
    /// The whole `if`/`while` condition chain (bool-member soft boost).
    pub(crate) bool_chain: bool,
    /// Value slot vs type-annotation slot (governs the enum universe).
    pub(crate) expr_position: bool,
    /// After `yield`/`yield*` -- coroutine fns promote. Expr position only.
    pub(crate) after_yield: bool,
    /// Param TYPE slot `fn f(name: Ty|)`: (param name, enclosing fn name)
    /// -- the Entity<T> idiom input. Type position only.
    pub(crate) param_type: Option<(String, String)>,
    /// Direct-call named-arg slot (`f(na|)` offers `name:` items).
    pub(crate) named_arg: Option<crate::cursor_context::NamedArgSlot>,
    /// First generic arg of `Coroutine<|>` (CoFrame synth).
    pub(crate) coroutine_generic: bool,
    /// Match-arm pattern HEAD: the byte anchor of the enclosing `match`'s
    /// scrutinee (its last byte). The slot expects the scrutinee's enum --
    /// variants rank first and patterns bind (no payload template).
    pub(crate) pattern_scrutinee: Option<usize>,
    /// `for x in |` iterable slot -- iterable-typed candidates rank first.
    pub(crate) for_in: bool,
}

/// Classify the cursor for completion: parse the munge ladder once, consult
/// every classifier once, and encode the cross-classifier guards (field
/// value yields to call arg; yield/Entity hints are universe-gated) in one
/// place instead of scattered call sites.
pub(crate) fn classify(src: &str, offset: usize) -> CompletionCx {
    let ast = crate::cursor_context::CursorAst::at(src, offset);
    let cx = ast.as_ref();
    let arg_call = cx.and_then(|c| c.call_arg());
    let expr_position = !cx.is_some_and(|c| c.is_type_position());
    CompletionCx {
        member: cx.and_then(|c| c.member_path()),
        member_recv_end: cx.and_then(|c| c.member_receiver_end()),
        enum_record: cx.and_then(|c| c.enum_record()),
        qualified: cx.and_then(|c| c.qualified_base()),
        struct_literal: cx.and_then(|c| c.struct_literal()),
        decorator: decorator_context(src, offset),
        field_value: if arg_call.is_none() {
            cx.and_then(|c| c.struct_field_value())
        } else {
            None
        },
        anchor: cx.and_then(slot_anchor),
        bool_chain: cx.is_some_and(|c| c.bool_condition_chain()),
        after_yield: expr_position && cx.is_some_and(|c| c.after_yield()),
        param_type: if expr_position { None } else { cx.and_then(|c| c.param_type()) },
        named_arg: cx.and_then(|c| c.named_arg_slot()),
        coroutine_generic: cx.is_some_and(|c| c.coroutine_first_generic_arg()),
        pattern_scrutinee: match_arm_scrutinee(src, offset),
        for_in: for_in_iterable_slot(src, offset),
        arg_call,
        expr_position,
        ast,
    }
}

impl CompletionCx {
    /// The typed expected-slot session, resolved in context precedence:
    /// call-arg slot (UFCS-receiver-filtered), struct-literal field value,
    /// then the slot anchors. The ONE expected-type deriver for the general
    /// path; the member path resolves the same `arg_call`/`anchor` inputs
    /// against its variant workspace.
    pub(crate) fn expected<'a>(
        &self,
        ws: &'a Workspace,
        mid: ModuleId,
        inf: &'a InferenceResult,
        offset: usize,
    ) -> Option<ExpectedArg<'a>> {
        if let Some((callee, idx, _recv)) = self.arg_call.as_ref() {
            // Receiver type for UFCS overload filtering: structural from the
            // classifier (a chain receiver's type is its callee's unique
            // return type, no paren rescanning).
            let recv_ty = self.ast.as_ref().and_then(|cx| {
                cx.call_receiver().and_then(|a| match a {
                    crate::cursor_context::ReceiverAnchor::ChainCall(name) => {
                        fn_unique_return_ty(ws, &name)
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
            // `Name { field: <cursor> }`: the field's declared type. Inside a
            // lambda body assigned to a function-typed field
            // (`is_open: () => x|`), the body returns the field's RETURN type.
            let slot = if *is_lambda {
                crate::query::struct_field_fn_return_ty(ws, sname, fname)
                    .or_else(|| crate::query::struct_field_ty(ws, sname, fname))
            } else {
                crate::query::struct_field_ty(ws, sname, fname)
            }?;
            return ExpectedArg::for_slot(ws, mid, inf, offset, slot);
        }
        // A bare-identifier assign target (`dest = |`) is typed `()` in place
        // position, so the byte anchor yields no type; resolve the slot from the
        // binding by name. Guarded by the same precedence `slot_anchor` uses --
        // a nested `==` RHS is the more specific slot and wins.
        if let Some(cx) = self.ast.as_ref() {
            if cx.comparison_lhs().is_none() {
                if let Some(name) = cx.assign_target_ident() {
                    if let Some(ty) = crate::query::binding_ty_by_name(ws, mid, inf, offset, &name) {
                        return ExpectedArg::for_slot(ws, mid, inf, offset, ty);
                    }
                }
            }
        }
        self.anchor.and_then(|a| expected_for_anchor(ws, mid, inf, offset, a))
    }
}

/// Where the expected type of the current slot comes from, beyond the
/// call-arg / struct-field cases: a byte anchor whose `type_at` IS the slot
/// type (assign LHS end, comparison LHS end, typed-`let` binding ident), or
/// the enclosing fn's declared return type. Computed ONCE from the cursor
/// classifier and consumed by BOTH the general and the member path, so the
/// two cannot drift. Anchors END before the cursor, so they resolve
/// identically against the live workspace and any completion-munge variant
/// (the munges only rewrite at/after the cursor).
#[derive(Clone, Copy)]
pub(crate) enum SlotAnchor {
    TypeAt(usize),
    EnclosingFnReturn,
}

/// Classify the slot-anchored expected-type context at the cursor.
/// Comparison first (a `==` nested in an assignment RHS is the more specific
/// slot), then assignment RHS, typed-`let` init, and `return` last.
pub(crate) fn slot_anchor(cx: &crate::cursor_context::CursorAst) -> Option<SlotAnchor> {
    cx.comparison_lhs()
        .or_else(|| cx.assign_lhs())
        .or_else(|| cx.typed_let_binding())
        .map(SlotAnchor::TypeAt)
        .or_else(|| {
            (cx.in_return_value() || cx.in_tail_return()).then_some(SlotAnchor::EnclosingFnReturn)
        })
}
