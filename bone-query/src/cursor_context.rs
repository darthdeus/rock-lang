//! AST-based cursor classification for completion
//! (docs/cursor-context-plan.md).
//!
//! Parses the MUNGED buffer (skull-parser is total: best-effort tree +
//! recovered errors) and answers "what is the cursor, structurally" from the
//! recovered AST -- replacing the backward byte-scanners one query at a
//! time. Invariant relied on throughout: the sentinel is injected AT the
//! cursor, so every span that STARTS before the cursor has identical offsets
//! in the munged and live buffers -- receiver spans from here feed `type_at`
//! on the LIVE workspace directly.

use skull_parser::ast::{CallSyntax, ExprData, ExprKind, IfLetClause, Pattern, StatementKind};
use skull_parser::Idx;
use ustr::Ustr;

pub(crate) struct CursorAst {
    ast: skull_parser::Ast,
    /// The candidate text the winning parse ran on -- span offsets in the
    /// AST index into THIS buffer, not the live one.
    text: String,
    /// Where to look up the cursor node. Equal to the live cursor offset for
    /// the insertion munges (the sentinel starts there); the RUN START for
    /// the token-replacement munge (the sentinel replaces the whole run).
    probe: usize,
}

/// Replace the identifier/number run around the cursor with the sentinel.
/// Recovers the digit-glue class: insertion produces `1__rock_completion__`,
/// which fails tokenize and drops the statement; replacement yields a clean
/// ident. Returns the munged source + the probe offset (the run's start).
fn token_replacement_munge(src: &str, offset: usize) -> Option<(String, usize)> {
    let b = src.as_bytes();
    let tok = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut s = offset.min(b.len());
    while s > 0 && tok(b[s - 1]) {
        s -= 1;
    }
    let mut e = offset.min(b.len());
    while e < b.len() && tok(b[e]) {
        e += 1;
    }
    if s == e {
        return None; // nothing to replace; the insertion munges cover it
    }
    let mut out = String::with_capacity(src.len() + 24);
    out.push_str(&src[..s]);
    out.push_str("__rock_completion__");
    out.push_str(&src[e..]);
    Some((crate::backend::balance_delimiters(out), s))
}

/// Offset of the `}` matching the `{` at `open` (None when unbalanced).
fn matching_brace(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (i, &c) in b.iter().enumerate().skip(open) {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Structural anchor for a UFCS call's receiver (docs/cursor-context-plan.md
/// flip 6): how to resolve the receiver's type without byte-rescanning.
pub(crate) enum ReceiverAnchor {
    /// Receiver ends here; `type_at` on its last byte resolves it (variable /
    /// field path -- survives the broken trailing arg).
    End(usize),
    /// The receiver is itself a call (`a.b().m(|`): its type is this
    /// callee's unique return type, resolved structurally (no inference).
    ChainCall(String),
}

/// Named-arg completion context: the direct callee, the count of leading
/// positional args already supplied, and the explicit `name:` args already
/// supplied (both excluding the slot being typed).
pub(crate) struct NamedArgSlot {
    pub callee: String,
    pub positional: usize,
    pub named: Vec<String>,
}

enum RecordHead {
    /// (name, name-ident end offset)
    Struct(String, usize),
    /// (enum, variant, variant-ident end offset)
    EnumVariant(String, String, usize),
}

impl RecordHead {
    fn end(&self) -> usize {
        match self {
            RecordHead::Struct(_, e) | RecordHead::EnumVariant(_, _, e) => *e,
        }
    }
}

struct RecordCtx {
    head: RecordHead,
    /// Offset of the literal's `{` (starts before the cursor, so live-valid).
    brace: usize,
    /// `(field name, cursor-in-lambda-body)` when in a field VALUE slot.
    value_field: Option<(String, bool)>,
}

impl CursorAst {
    /// Parse the best munge of `src` at `offset`. Ladder: the two insertion
    /// munges, the token-replacement munge, then the raw buffer -- preferring
    /// the first candidate whose parse actually KEEPS the cursor region (a
    /// candidate can parse while recovery drops the cursor's statement).
    /// None only when every rung fails to tokenize -- callers keep their byte
    /// fallback there.
    pub(crate) fn at(src: &str, offset: usize) -> Option<CursorAst> {
        let file = Ustr::from("<cursor>");
        let mut candidates: Vec<(String, usize)> = crate::backend::completion_munges(src, offset)
            .into_iter()
            .map(|m| (m, offset))
            .collect();
        candidates.extend(token_replacement_munge(src, offset));
        // Sentinel + comma: a new arg typed BEFORE an existing expr
        // (`f(a, <cursor> (x) => ..)`) -- plain insertion juxtaposes the
        // sentinel with the next expr and fails to parse.
        {
            let (head, tail) = src.split_at(offset.min(src.len()));
            let mut s = String::with_capacity(src.len() + 24);
            s.push_str(head);
            s.push_str("__rock_completion__,");
            s.push_str(tail);
            candidates.push((crate::backend::balance_delimiters(s), offset));
        }
        // Arm-completing sentinel: `E::<cursor>` in a FRESH match arm (no
        // `=>` typed yet) -- every plain munge leaves an incomplete arm that
        // recovery drops. `<sentinel> => {}` parses it as the pattern it is;
        // in non-arm positions this rung just fails and falls through.
        {
            let (head, tail) = src.split_at(offset.min(src.len()));
            let mut s = String::with_capacity(src.len() + 32);
            s.push_str(head);
            s.push_str("__rock_completion__ => {}");
            s.push_str(tail);
            candidates.push((crate::backend::balance_delimiters(s), offset));
        }
        candidates.push((src.to_string(), offset));
        // A candidate can parse while recovery mangles the cursor region:
        // the sentinel ends up an ORPHAN with no parent expr (digit-glue), or
        // attached to a hallucinated node (`<sentinel> (x) => ..` /
        // `<sentinel> c: 3` parse as a call WITH THE SENTINEL AS CALLEE, plus
        // recovery errors). Accept immediately only a CLEAN parse with the
        // cursor attached; otherwise grade candidates (attached-with-errors >
        // sentinel-as-callee > orphan > empty) in ladder order -- a file with
        // unrelated pre-existing errors still classifies.
        let mut attached: Option<CursorAst> = None;
        let mut hallucinated: Option<CursorAst> = None;
        let mut orphan: Option<CursorAst> = None;
        let mut empty: Option<CursorAst> = None;
        for (cand, probe) in candidates {
            let Ok(ast) = skull_parser::parse_module(&cand, file) else { continue };
            let clean = ast.recovery_errors.is_empty();
            let cx = CursorAst { ast, text: cand, probe };
            let chain = cx.expr_chain();
            if chain.is_empty() {
                if empty.is_none() {
                    empty = Some(cx);
                }
                continue;
            }
            // A lone node STARTING at the probe is sentinel-derived (the bare
            // sentinel ident, or a hallucinated `<sentinel> {}` literal) --
            // a true orphan. A lone node ENCLOSING the probe is real
            // structure: e.g. the raw buffer's parse of `f(a, k: v,| b)` has
            // only the statement-level call containing the cursor -- exactly
            // the between-args call-arg shape.
            let is_sentinel_orphan =
                chain.len() == 1 && cx.expr(chain[0]).span.offset_range.0 >= probe;
            if is_sentinel_orphan {
                if orphan.is_none() {
                    orphan = Some(cx);
                }
                continue;
            }
            if clean {
                return Some(cx);
            }
            let halluc = chain.len() >= 2
                && matches!(&cx.expr(chain[1]).kind,
                    ExprKind::FunctionCall { func, .. } if *func == chain[0]);
            if halluc {
                if hallucinated.is_none() {
                    hallucinated = Some(cx);
                }
            } else if attached.is_none() {
                attached = Some(cx);
            }
        }
        attached.or(hallucinated).or(orphan).or(empty)
    }

    fn expr(&self, i: Idx<ExprData>) -> &ExprData {
        &self.ast.exprs[i]
    }

    /// Exprs whose span contains the cursor, innermost first. Span nesting is
    /// ancestry in a well-formed parse (sibling spans are disjoint, so at most
    /// one sibling contains the cursor) -- no parent map needed.
    fn expr_chain(&self) -> Vec<Idx<ExprData>> {
        let off = self.probe;
        let mut v: Vec<(usize, Idx<ExprData>)> = self
            .ast
            .exprs
            .iter()
            .filter(|(_, e)| {
                let (s, t) = e.span.offset_range;
                s <= off && off < t
            })
            .map(|(i, e)| (e.span.offset_range.1 - e.span.offset_range.0, i))
            .collect();
        v.sort_by_key(|(len, _)| *len);
        v.into_iter().map(|(_, i)| i).collect()
    }

    /// `yield <cursor>` / `yield* <cursor>`: the cursor node is the DIRECT
    /// operand of a yield (matching the byte scanner's "immediately after
    /// `yield`" semantics -- `yield foo.b|` is member context, not this).
    pub(crate) fn after_yield(&self) -> bool {
        let chain = self.expr_chain();
        chain.windows(2).any(|w| match self.expr(w[1]).kind {
            ExprKind::Yield(Some(op)) | ExprKind::YieldDelegate(op) => op == w[0],
            _ => false,
        })
    }

    /// `Enum::<cursor>` -- the qualified base when the cursor sits in the
    /// member slot. Three surface forms produce it:
    /// - `QualifiedIdent` (when the parser resolves `E::x` as one),
    /// - `TypeAscription` -- `E::<sentinel>` parses as `E :: <Type>` (the
    ///   `::` ascription ambiguity), cursor inside the ascribed type,
    /// - EnumVariant/Record PATTERNS (match arms, if/while-let), which live
    ///   outside the exprs arena.
    pub(crate) fn qualified_base(&self) -> Option<String> {
        if let Some(node) = self.expr_chain().first().copied() {
            match &self.expr(node).kind {
                ExprKind::QualifiedIdent { base, member } => {
                    if member.span.offset_range.0 <= self.probe {
                        return Some(base.name.to_string());
                    }
                }
                ExprKind::TypeAscription(lhs, ty) => {
                    let (s, e) = self.ast.types[*ty].span.offset_range;
                    if s <= self.probe && self.probe < e {
                        if let ExprKind::Ident(base) = &self.expr(*lhs).kind {
                            return Some(base.name.to_string());
                        }
                    }
                }
                // Cursor mid-variant-ident: the munge glues the sentinel into
                // one identifier, so the record literal parses whole and the
                // record node itself is the innermost expr at the cursor.
                ExprKind::EnumVariantRecord { enum_name, variant_name, .. } => {
                    let (s, e) = variant_name.span.offset_range;
                    if s <= self.probe && self.probe < e {
                        return Some(enum_name.name.to_string());
                    }
                }
                _ => {}
            }
        }
        self.pattern_qualified_base()
    }

    /// Visit every pattern in the module (match arms, if/while-let bindings),
    /// including nested sub-patterns.
    fn walk_patterns(&self, f: &mut dyn FnMut(&Pattern)) {
        fn walk(p: &Pattern, f: &mut dyn FnMut(&Pattern)) {
            f(p);
            match p {
                Pattern::Tuple(subs) | Pattern::Or(subs) => {
                    for sub in subs {
                        walk(sub, f);
                    }
                }
                Pattern::EnumVariant { args: Some(args), .. } => {
                    for sub in args {
                        walk(sub, f);
                    }
                }
                Pattern::Record { fields, .. } => {
                    for (_, sub) in fields {
                        walk(sub, f);
                    }
                }
                _ => {}
            }
        }
        for (_, e) in self.ast.exprs.iter() {
            match &e.kind {
                ExprKind::Match { arms, .. } => {
                    for arm in arms {
                        walk(&arm.pattern, f);
                    }
                }
                ExprKind::IfLet { pattern, .. } => walk(pattern, f),
                ExprKind::IfLetChain { clauses, .. } => {
                    for c in clauses {
                        if let IfLetClause::Binding { pattern, .. } = c {
                            walk(pattern, f);
                        }
                    }
                }
                _ => {}
            }
        }
        for (_, st) in self.ast.stmts.iter() {
            if let StatementKind::WhileLet(wl) = &st.kind {
                walk(&wl.pattern, f);
            }
        }
    }

    /// `Enum::<cursor>` inside a PATTERN (match arm, if/while-let binding).
    fn pattern_qualified_base(&self) -> Option<String> {
        let off = self.probe;
        let mut found = None;
        self.walk_patterns(&mut |p| {
            let (base, vname) = match p {
                Pattern::EnumVariant { enum_name: Some(base), variant_name, .. } => {
                    (base, variant_name)
                }
                Pattern::Record { enum_name: Some(base), name: Some(vname), .. } => (base, vname),
                _ => return,
            };
            let (s, e) = vname.span.offset_range;
            if s <= off && off < e {
                found = Some(base.name.to_string());
            }
        });
        found
    }

    /// `Enum::Variant { <cursor> }` as a record PATTERN (match arm,
    /// if/while-let): `(enum, variant)` when the cursor is inside the braces.
    fn pattern_record(&self) -> Option<(String, String)> {
        let mut found = None;
        self.walk_patterns(&mut |p| {
            let Pattern::Record { enum_name: Some(base), name: Some(vname), .. } = p else {
                return;
            };
            let ve = vname.span.offset_range.1;
            let Some(brace) = self.text[ve..].find('{').map(|i| ve + i) else { return };
            let Some(close) = matching_brace(self.text.as_bytes(), brace) else { return };
            if brace < self.probe && self.probe <= close {
                found = Some((base.name.to_string(), vname.name.to_string()));
            }
        });
        found
    }

    /// Cursor inside a struct DECLARATION field's DEFAULT expr
    /// (`struct S { dur: float = 0.|0 }`): `(struct, field, is_lambda_body)`.
    fn decl_field_default(&self) -> Option<(String, String, bool)> {
        for item in &self.ast.items {
            let skull_parser::ast::ItemKind::Struct(sd) = &item.kind else { continue };
            for f in &sd.fields {
                let Some(d) = f.default else { continue };
                let v = self.expr(d);
                let (vs, ve) = v.span.offset_range;
                if vs <= self.probe && self.probe < ve {
                    return Some((
                        sd.name.name.to_string(),
                        f.name.name.to_string(),
                        self.lambda_body_contains(v),
                    ));
                }
            }
        }
        None
    }

    /// Whether `v` is a lambda whose BODY block contains the cursor.
    fn lambda_body_contains(&self, v: &ExprData) -> bool {
        matches!(&v.kind, ExprKind::Lambda { body, .. }
            if {
                let (bs, be) = self.ast.blocks[*body].span.offset_range;
                bs <= self.probe && self.probe < be
            })
    }

    /// Enclosing record literal (struct literal or `Enum::Variant { .. }`)
    /// with the cursor inside its BRACES, innermost first. `value_field` is
    /// set when the cursor sits in a field's VALUE expr (not a pun -- a pun's
    /// "value" is the written field name itself, i.e. a NAME slot).
    fn record_ctx(&self) -> Option<RecordCtx> {
        for &node in self.expr_chain().iter() {
            let (head, fields) = match &self.expr(node).kind {
                ExprKind::StructLiteral { name, fields } => {
                    (RecordHead::Struct(name.name.to_string(), name.span.offset_range.1), fields)
                }
                ExprKind::EnumVariantRecord { enum_name, variant_name, fields } => (
                    RecordHead::EnumVariant(
                        enum_name.name.to_string(),
                        variant_name.name.to_string(),
                        variant_name.span.offset_range.1,
                    ),
                    fields,
                ),
                _ => continue,
            };
            let head_end = head.end();
            let Some(brace) = self.text[head_end..].find('{').map(|i| head_end + i) else {
                continue;
            };
            if self.probe <= brace {
                continue; // cursor in the head, not the braces
            }
            let mut value_field: Option<(String, bool)> = None;
            for (path, val) in fields {
                let v = self.expr(*val);
                let (vs, ve) = v.span.offset_range;
                if vs <= self.probe && self.probe < ve {
                    let pun = path.len() == 1 && path[0].span.offset_range == v.span.offset_range;
                    if !pun {
                        let fname = path.last()?.name.to_string();
                        value_field = Some((fname, self.lambda_body_contains(v)));
                    }
                    break;
                }
            }
            return Some(RecordCtx { head, brace, value_field });
        }
        None
    }

    /// `Name { <cursor> }` field-NAME slot: `(struct name, brace offset)`.
    /// None in a field-VALUE slot (type-directed completion handles those).
    pub(crate) fn struct_literal(&self) -> Option<(String, usize)> {
        let c = self.record_ctx()?;
        if c.value_field.is_some() {
            return None;
        }
        match c.head {
            RecordHead::Struct(name, _) => Some((name, c.brace)),
            RecordHead::EnumVariant(..) => None,
        }
    }

    /// `Enum::Variant { <cursor> }`: `(enum, variant)`. Fires for any slot
    /// inside the braces -- parity with the byte scanner, which serves field
    /// names there regardless of slot -- and inside record PATTERNS, where
    /// field names are just as completable.
    pub(crate) fn enum_record(&self) -> Option<(String, String)> {
        match self.record_ctx().map(|c| c.head) {
            Some(RecordHead::EnumVariant(e, v, _)) => Some((e, v)),
            Some(RecordHead::Struct(..)) => None,
            None => self.pattern_record(),
        }
    }

    /// `Name { field: <cursor> }` field-VALUE slot: `(struct, field,
    /// is_lambda_body)` -- is_lambda_body when the value is a lambda and the
    /// cursor sits in its BODY (type-direct on the fn's return type).
    pub(crate) fn struct_field_value(&self) -> Option<(String, String, bool)> {
        match self.record_ctx() {
            Some(c) => {
                let (field, is_lambda) = c.value_field?;
                match c.head {
                    RecordHead::Struct(name, _) => Some((name, field, is_lambda)),
                    RecordHead::EnumVariant(..) => None,
                }
            }
            // A struct DECL field's default expr type-directs the same way.
            None => self.decl_field_default(),
        }
    }

    /// Enclosing call with the cursor in an ARGUMENT slot: `(callee,
    /// arg_index, receiver_end)`. The UFCS receiver counts as param 0 (so a
    /// paren-arg position shifts by one) and `receiver_end` is the receiver
    /// expr's end offset -- the anchor `method_receiver_ty` resolves the
    /// receiver's type from. Argument position is STRUCTURAL (the arg node
    /// the cursor sits in), so commas inside strings / nested calls / arrays
    /// never miscount, unlike the byte scanner.
    pub(crate) fn call_arg(&self) -> Option<(String, u32, Option<usize>)> {
        let (call, _cursor_arg, pos) = self.enclosing_arg_call()?;
        let ExprKind::FunctionCall { func, .. } = &self.expr(call).kind else {
            return None;
        };
        match &self.expr(*func).kind {
            ExprKind::Ident(id) => Some((id.name.to_string(), pos as u32, None)),
            ExprKind::QualifiedIdent { member, .. } => {
                Some((member.name.to_string(), pos as u32, None))
            }
            ExprKind::FieldAccess(recv, method) | ExprKind::SafeNavigation(recv, method) => {
                let recv_end = self.expr(*recv).span.offset_range.1;
                Some((method.name.to_string(), pos as u32 + 1, Some(recv_end)))
            }
            _ => None,
        }
    }

    /// The normal/`@` call the cursor sits in an arg slot of, plus the cursor's
    /// OWN arg node (`None` when between args) and the positional slot index.
    /// Shared by `call_arg` and `named_arg_slot`. `None` when the cursor isn't
    /// in a paren-arg position (operator/no-parens call, callee region, or a
    /// param-list declaration slot).
    fn enclosing_arg_call(&self) -> Option<(Idx<ExprData>, Option<Idx<ExprData>>, usize)> {
        let chain = self.expr_chain();
        for (ci, &node) in chain.iter().enumerate() {
            // A lambda between the cursor and any enclosing call, with the
            // cursor OUTSIDE its body, means the cursor declares a param (or
            // return type): a declaration position, not a value slot -- no
            // arg context (F3 flip-5 design decision).
            if matches!(&self.expr(node).kind, ExprKind::Lambda { .. })
                && !self.lambda_body_contains(self.expr(node))
            {
                return None;
            }
            let ExprKind::FunctionCall { func, args, syntax } = &self.expr(node).kind else {
                continue;
            };
            // Operator calls (`a + |`) and the no-parens surface form are not
            // paren-arg contexts for the byte scanner; keep parity for now
            // (NoParens is a candidate improvement, noted in the plan doc).
            if !matches!(syntax, CallSyntax::Normal | CallSyntax::At) {
                continue;
            }
            if ci == 0 {
                // The call node ITSELF is the innermost expr at the cursor: a
                // fresh arg is being typed between existing args (`f(a, k: v,|
                // b: w)`). Every sentinel munge breaks there (juxtaposition /
                // positional-after-named), the raw parse wins the ladder, and
                // no arg node contains the cursor -- the slot is the count of
                // args ending at or before the probe. Guard: the probe must
                // sit past the arg list's `(`, not in the callee region.
                let func_end = self.expr(*func).span.offset_range.1;
                let Some(paren) = self.text[func_end..].find('(').map(|i| func_end + i)
                else {
                    continue;
                };
                if self.probe <= paren {
                    continue;
                }
                let pos = args
                    .iter()
                    .filter(|a| self.expr(a.value).span.offset_range.1 <= self.probe)
                    .count();
                return Some((node, None, pos));
            } else {
                let inner = chain[ci - 1];
                if *func == inner {
                    continue; // cursor in the callee, not an arg
                }
                let Some(pos) = args.iter().position(|a| a.value == inner) else {
                    continue;
                };
                return Some((node, Some(inner), pos));
            }
        }
        None
    }

    /// Named-arg completion context (`greet(gr|)` -> callee `greet`, plus the
    /// args already supplied). `positional` counts the leading unnamed args
    /// (they bind params 0..positional by declaration order); `named` lists the
    /// explicit `name:` args -- both EXCLUDING the slot being typed. `None` for
    /// UFCS/method callees (the receiver occupies param 0, deferred) and outside
    /// a direct call's arg list. The caller resolves the callee's param list and
    /// offers the still-unsupplied ones.
    pub(crate) fn named_arg_slot(&self) -> Option<NamedArgSlot> {
        let (call, cursor_arg, _pos) = self.enclosing_arg_call()?;
        let ExprKind::FunctionCall { func, args, .. } = &self.expr(call).kind else {
            return None;
        };
        let callee = match &self.expr(*func).kind {
            ExprKind::Ident(id) => id.name.to_string(),
            ExprKind::QualifiedIdent { member, .. } => member.name.to_string(),
            _ => return None,
        };
        let mut positional = 0usize;
        let mut named: Vec<String> = Vec::new();
        for a in args.iter() {
            if cursor_arg == Some(a.value) {
                continue; // the slot being typed, not a supplied arg
            }
            // Between-args (no cursor node): only args ENDING before the probe
            // are supplied for this slot; a trailing arg after it isn't. With a
            // cursor node every OTHER arg is already present, so count them all.
            if cursor_arg.is_none() && self.expr(a.value).span.offset_range.1 > self.probe {
                continue;
            }
            match &a.name {
                Some(n) => named.push(n.name.to_string()),
                None => positional += 1,
            }
        }
        Some(NamedArgSlot { callee, positional, named })
    }

    /// TYPE-vs-expression slot: true when the innermost node containing the
    /// probe is a TYPE (`let x: F|`, `fn f(p: F|)`, `-> F|`, `Vec<F|>`,
    /// `struct S { f: F| }`). Positive evidence only -- no containing type
    /// node means expression.
    pub(crate) fn is_type_position(&self) -> bool {
        let ty = self
            .ast
            .types
            .iter()
            .map(|(_, t)| t.span.offset_range)
            .filter(|(s, e)| *s <= self.probe && self.probe < *e)
            .min_by_key(|(s, e)| e - s);
        let Some((ts, te)) = ty else { return false };
        match self.expr_chain().first() {
            None => true,
            Some(&e) => {
                // Types nest inside exprs (ascription, casts): the smaller
                // span is the slot the cursor is actually in.
                let (es, ee) = self.expr(e).span.offset_range;
                (te - ts) <= (ee - es)
            }
        }
    }

    /// `fn name(param: <cursor>)` -- the enclosing fn DECL's `(param name,
    /// fn name)` when the probe sits in a param's TYPE slot. Drives the
    /// Entity-type hint.
    pub(crate) fn param_type(&self) -> Option<(String, String)> {
        for item in &self.ast.items {
            let skull_parser::ast::ItemKind::Function(fd) = &item.kind else { continue };
            for p in &fd.params {
                let (ts, te) = self.ast.types[p.ty].span.offset_range;
                if ts <= self.probe && self.probe < te {
                    return Some((p.name.name.to_string(), fd.name.name.to_string()));
                }
            }
        }
        None
    }

    /// True when the probe sits in the FIRST generic argument of a
    /// `Coroutine<..>` type (`Coroutine<|`, `Coroutine<Co|`). Drives the
    /// CoFrame-first boost.
    pub(crate) fn coroutine_first_generic_arg(&self) -> bool {
        use skull_parser::ast::TypeExprKind;
        self.ast.types.iter().any(|(_, t)| {
            let TypeExprKind::App { ctor, args } = &t.kind else { return false };
            if ctor.name.as_str() != "Coroutine" {
                return false;
            }
            args.first().is_some_and(|&a| {
                let (s, e) = self.ast.types[a].span.offset_range;
                s <= self.probe && self.probe < e
            })
        })
    }

    /// The member chain the cursor is typing is the WHOLE condition of an
    /// `if`/`while` (`if thing.foo.bar|`) -- the bool-field soft-boost
    /// context. `if a && thing.foo|` is not (the condition is the `&&`).
    pub(crate) fn bool_condition_chain(&self) -> bool {
        let Some(&top) = self.expr_chain().first() else { return false };
        if !matches!(
            self.expr(top).kind,
            ExprKind::FieldAccess(..) | ExprKind::SafeNavigation(..) | ExprKind::Ident(..)
        ) {
            return false;
        }
        let in_if = self.ast.exprs.iter().any(|(_, e)| {
            matches!(&e.kind, ExprKind::If { branches, .. }
                if branches.iter().any(|(c, _)| *c == top))
        });
        in_if
            || self.ast.stmts.iter().any(|(_, st)| {
                matches!(&st.kind, StatementKind::While(w) if w.condition == top)
            })
    }

    /// The dotted receiver path when the cursor types a MEMBER
    /// (`line.start.<cursor>` -> `["line", "start"]`). Mirrors the byte
    /// scanner's contract: pure ident chains only (`?.` links included);
    /// None for complex receivers (`foo().bar.|`), which the member-path
    /// consumer can't express as segments anyway.
    pub(crate) fn member_path(&self) -> Option<Vec<String>> {
        let chain = self.expr_chain();
        let &node = chain.first()?;
        let (ExprKind::FieldAccess(recv, member) | ExprKind::SafeNavigation(recv, member)) =
            &self.expr(node).kind
        else {
            return None;
        };
        if self.probe < member.span.offset_range.0 {
            return None; // cursor in the receiver, not the member slot
        }
        let mut segs: Vec<String> = Vec::new();
        let mut cur = *recv;
        loop {
            match &self.expr(cur).kind {
                ExprKind::Ident(id) => {
                    segs.push(id.name.to_string());
                    break;
                }
                ExprKind::FieldAccess(r, m) | ExprKind::SafeNavigation(r, m) => {
                    segs.push(m.name.to_string());
                    cur = *r;
                }
                _ => return None,
            }
        }
        segs.reverse();
        Some(segs)
    }

    /// The UFCS receiver of the call whose arg slot the cursor sits in, as a
    /// structural anchor. None when the cursor isn't in a method call's args
    /// (matches `call_arg`'s Some-with-receiver cases exactly).
    pub(crate) fn call_receiver(&self) -> Option<ReceiverAnchor> {
        // The receiver anchor only exists when call_arg found a UFCS call.
        self.call_arg()?.2?;
        let chain = self.expr_chain();
        for &node in chain.iter() {
            let ExprKind::FunctionCall { func, syntax, .. } = &self.expr(node).kind else {
                continue;
            };
            if !matches!(syntax, CallSyntax::Normal | CallSyntax::At) {
                continue;
            }
            let (ExprKind::FieldAccess(recv, _) | ExprKind::SafeNavigation(recv, _)) =
                &self.expr(*func).kind
            else {
                continue;
            };
            let r = self.expr(*recv);
            return Some(match &r.kind {
                // `a.b().m(|`: the receiver is a call -- its type is the last
                // method's unique return type.
                ExprKind::FunctionCall { func: rf, syntax: rs, .. }
                    if matches!(rs, CallSyntax::Normal | CallSyntax::At) =>
                {
                    match &self.expr(*rf).kind {
                        ExprKind::Ident(id) => ReceiverAnchor::ChainCall(id.name.to_string()),
                        ExprKind::FieldAccess(_, m) | ExprKind::SafeNavigation(_, m) => {
                            ReceiverAnchor::ChainCall(m.name.to_string())
                        }
                        _ => ReceiverAnchor::End(r.span.offset_range.1),
                    }
                }
                _ => ReceiverAnchor::End(r.span.offset_range.1),
            });
        }
        None
    }
}

#[cfg(test)]
mod gaps {
    use super::*;

    fn cx(marked: &str) -> (String, usize, CursorAst) {
        let off = marked.find("$0").expect("cursor marker");
        let src = marked.replacen("$0", "", 1);
        let cx = CursorAst::at(&src, off).expect("classifiable");
        (src, off, cx)
    }

    #[test]
    fn call_arg_survives_cursor_adjacent_to_a_number() {
        // Digit-glue: the insertion munges produce `1__rock_completion__`,
        // which fails tokenize -- the token-REPLACEMENT munge recovers.
        let (_, _, cx) = cx("fn feed(x: int) {}\nfn t() {\n  feed(1$0)\n}\n");
        assert_eq!(cx.call_arg(), Some(("feed".to_string(), 0, None)));
    }

    #[test]
    fn after_yield_survives_cursor_after_a_number() {
        let (_, _, cx) = cx("co fn c() -> Coroutine<int> {\n  yield 1$0\n}\n");
        assert!(cx.after_yield());
    }

    #[test]
    fn qualified_base_in_a_match_arm_pattern() {
        // Patterns live outside the exprs arena -- the walk must cover them.
        let (_, _, cx) =
            cx("enum E { X, Y }\nfn t(e: E) -> int {\n  match e {\n    E::$0X => 1,\n    _ => 2,\n  }\n}\n");
        assert_eq!(cx.qualified_base(), Some("E".to_string()));
    }

    #[test]
    fn call_arg_survives_a_following_lambda_arg() {
        // Sentinel juxtaposed with the NEXT arg's expr fails to parse
        // (`__rock_completion__ (x: int) => ..`); the sentinel-plus-comma
        // munge recovers -- new arg being typed before an existing one.
        let (_, _, cx) = cx(
            "fn apply(n: int, f: (int) -> int) -> int { return f(n) }\nfn t() {\n  apply(3, $0(x: int) => x * x)\n}\n",
        );
        assert_eq!(cx.call_arg(), Some(("apply".to_string(), 1, None)));
    }

    #[test]
    fn qualified_base_in_a_record_variant_literal() {
        // `Effect::|Bandage { .. }` parses as an EnumVariantRecord EXPR --
        // needs its own arm (neither QualifiedIdent nor a pattern).
        let (_, _, cx) = cx(
            "enum E { B { d: int } }\nfn t() {\n  let v = E::$0B { d: 1 }\n}\n",
        );
        assert_eq!(cx.qualified_base(), Some("E".to_string()));
    }

    #[test]
    fn call_arg_between_named_args() {
        // Fresh arg being typed after a comma in a named-arg call: every
        // sentinel munge breaks (juxtaposition / positional-after-named), so
        // the RAW parse wins the ladder -- and the cursor sits BETWEEN arg
        // nodes, inside only the call node itself.
        let (_, _, cx) = cx(
            "fn draw(a: int, b: int, c: int) {}\nfn t() {\n  draw(1, b: 2,$0 c: 3)\n}\n",
        );
        assert_eq!(cx.call_arg(), Some(("draw".to_string(), 2, None)));
    }

    #[test]
    fn call_arg_suppressed_in_a_lambda_param_list() {
        // F3 flip-5 design decision: `apply(3, (x$0) => ..)` -- the cursor
        // declares a lambda PARAM; it is not a value slot of `apply`, so no
        // arg context (no type-directed ranking, no field-ref path). The
        // byte scanner miscounted this as arg 1.
        let (_, _, cx) = cx(
            "fn apply(n: int, f: (int) -> int) -> int { return f(n) }\nfn t() {\n  apply(3, (x$0: int) => x * x)\n}\n",
        );
        assert_eq!(cx.call_arg(), None);
    }

    #[test]
    fn call_arg_fires_in_a_lambda_body() {
        // Control: the lambda BODY is a value expr -- arg context applies.
        let (_, _, cx) = cx(
            "fn apply(n: int, f: (int) -> int) -> int { return f(n) }\nfn t() {\n  apply(3, (x: int) => x$0)\n}\n",
        );
        assert_eq!(cx.call_arg(), Some(("apply".to_string(), 1, None)));
    }

    #[test]
    fn qualified_base_in_a_fresh_match_arm() {
        // `E::<cursor>` in a fresh arm with no `=>` typed yet: every plain
        // munge leaves an incomplete arm that recovery DROPS (empty chain).
        // The arm-completing rung (`<sentinel> => {}`) parses it as the
        // pattern it is.
        let (_, _, cx) = cx(
            "enum E { X, Y(int) }\nfn t(e: E) {\n  match e {\n    E::$0\n  }\n}\n",
        );
        assert_eq!(cx.qualified_base(), Some("E".to_string()));
    }

    #[test]
    fn qualified_base_mid_variant_ident_in_record_literal() {
        // Cursor MID-ident glues the sentinel into one identifier
        // (`B__rock_completion__andaging`), which parses CLEAN as an
        // EnumVariantRecord expr -- unlike the `::|B` boundary case, which
        // lands on TypeAscription. Needs its own arm.
        let (_, _, cx) = cx(
            "enum E { B { d: int, h: int } }\nfn t(h: int) {\n  let v = E::B$0andaging { d: 1, h }\n}\n",
        );
        assert_eq!(cx.qualified_base(), Some("E".to_string()));
    }

    #[test]
    fn enum_record_fires_in_a_record_pattern() {
        // `match e { StatusEffect::Bandaging {| heal_amount, .. } => .. }` --
        // field-name completion is just as useful in a record PATTERN as in a
        // literal; the byte scanner (brace-blind) serves it, so must we.
        let (_, _, cx) = cx(
            "enum E { B { d: int, h: int } }\nfn t(e: E) {\n  match e {\n    E::B { $0d, .. } => {}\n    _ => {}\n  }\n}\n",
        );
        assert_eq!(cx.enum_record(), Some(("E".to_string(), "B".to_string())));
    }

    #[test]
    fn struct_field_value_fires_in_a_decl_field_default() {
        // `struct S { dur: float = 0.|0 }` -- the default is a value expr
        // typed against the field's declared type; the byte scanner
        // type-directs there (it can't tell a decl from a literal) and the
        // behavior is worth keeping on purpose.
        let (_, _, cx) = cx(
            "struct S {\n  dur: float = 0.$00,\n  n: int = 1,\n}\n",
        );
        assert_eq!(
            cx.struct_field_value(),
            Some(("S".to_string(), "dur".to_string(), false))
        );
    }

    #[test]
    fn coroutine_first_arg_true_in_the_generic_slot() {
        let (_, _, cx) = cx("co fn c() -> Coroutine<$0> {\n  yield 1\n}\n");
        assert!(cx.coroutine_first_generic_arg());
    }

    #[test]
    fn coroutine_first_arg_false_in_a_comment() {
        // The byte scanner is comment-blind here; the classifier sees no type.
        let (_, _, cx) = cx("fn t() {\n  // Coroutine<x$0\n}\n");
        assert!(!cx.coroutine_first_generic_arg());
    }

    #[test]
    fn bool_condition_chain_true_for_a_whole_condition() {
        let (_, _, cx) = cx(
            "struct T { ok: bool }\nfn t(thing: T) {\n  if thing.o$0\n}\n",
        );
        assert!(cx.bool_condition_chain());
    }

    #[test]
    fn bool_condition_chain_false_under_a_conjunction() {
        let (_, _, cx) = cx(
            "struct T { ok: bool }\nfn t(a: bool, thing: T) {\n  if a && thing.o$0\n}\n",
        );
        assert!(!cx.bool_condition_chain());
    }

    #[test]
    fn qualified_base_in_expression_position() {
        // Control: the expression-position case the sweep also flagged --
        // isolates whether that class is a classifier bug or file-specific.
        let (_, _, cx) = cx("enum E { X }\nfn t() {\n  let v = E::$0X\n}\n");
        assert_eq!(cx.qualified_base(), Some("E".to_string()));
    }
}


#[cfg(test)]
mod frontier_census {
    use super::*;

    fn corpus() -> Vec<(String, String)> {
        let mut out = Vec::new();
        for dir in ["../tests", "../mag", "../bitgun"] {
            let Ok(rd) = std::fs::read_dir(dir) else { continue };
            let mut files: Vec<_> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "rock"))
                .collect();
            files.sort();
            // A capped, deterministic slice per dir keeps the sweep fast.
            for p in files.into_iter().take(12) {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    out.push((p.display().to_string(), text));
                }
            }
        }
        out
    }

    /// One-off, gated: how often does CursorAst::at return None -- the ONLY
    /// case the byte fallbacks still run. Sweeps a stride over EVERY byte
    /// offset (not just "interesting" ones).
    #[test]
    fn none_rate_over_all_offsets() {
        if std::env::var("ROCK_CTX_FRONTIER").is_err() {
            return;
        }
        let mut total = 0usize;
        let mut none = 0usize;
        let mut samples: Vec<String> = Vec::new();
        for (name, src) in corpus() {
            let mut off = 1usize;
            while off < src.len() {
                if src.is_char_boundary(off) {
                    total += 1;
                    if CursorAst::at(&src, off).is_none() {
                        none += 1;
                        if samples.len() < 10 {
                            let s = off.saturating_sub(40);
                            samples.push(format!("{name}:{off} ...{:?}", &src[s..off]));
                        }
                    }
                }
                off += 5;
            }
        }
        assert!(
            none == 0,
            "the tokenize frontier is supposed to be EMPTY (the raw buffer is \
             always a ladder rung and balance_delimiters closes broken string \
             tails) -- {none}/{total} positions returned None:\n{}",
            samples.join("\n")
        );
        assert!(total > 10_000, "census too small to mean anything: {total}");
    }
}

#[cfg(test)]
mod tokenize_frontier {
    use super::*;

    /// The mid-edit states whose RAW buffer fatally fails tokenize
    /// (unterminated strings / interpolations, bad numeric literals): the
    /// classifier must still classify via the balanced munges (or we must
    /// know it can't, byte fallbacks or not).
    #[test]
    fn broken_tail_states_still_classify() {
        let cases = [
            "fn t() {\n  let s = \"foo {",
            "fn t() {\n  let s = \"foo ",
            "fn t() {\n  let s = \"\"\"block\n  text",
            "fn t() {\n  let n = 0x",
            "fn t() {\n  let n = 0b",
            "fn t() {\n  let s = \"a {b.",
        ];
        for src in cases {
            let got = CursorAst::at(src, src.len());
            assert!(got.is_some(), "must classify (Some) for broken tail: {src:?}");
        }
    }
}

#[cfg(test)]
mod type_position_matrix {
    use super::*;

    /// `|` marks the cursor (removed). Ported from the byte scanner's case
    /// matrix when position.rs was deleted -- same coverage, classifier
    /// answers.
    fn classify(src: &str) -> bool {
        let at = src.find('|').expect("mark the cursor with `|`");
        let cleaned: String = src[..at].to_string() + &src[at + 1..];
        CursorAst::at(&cleaned, at).expect("classifiable").is_type_position()
    }

    #[test]
    fn type_positions() {
        assert!(classify("fn t() {\n  let x: F|\n}"), "let annotation");
        assert!(classify("fn f(p: F|) {}"), "param type");
        assert!(classify("fn f(a: int, p: F|) {}"), "later param type");
        assert!(classify("fn f() -> F| {}"), "return type");
        assert!(classify("struct S { field: F| }"), "struct field type");
        assert!(classify("global g: F|"), "global annotation");
        assert!(classify("fn t() {\n  let v: Vec<F|>\n}"), "generic argument");
        assert!(classify("fn t() {\n  let m: Map<K, V|>\n}"), "later generic argument");
        assert!(classify("fn t(y: int) {\n  let x = y as F|\n}"), "type cast");
        assert!(classify("fn t() {\n  let a: [F|]\n}"), "array type element");
        assert!(classify("enum E { V(F|) }"), "tuple-variant payload");
    }

    #[test]
    fn expression_positions() {
        assert!(!classify("fn t() {\n  let x = F|\n}"), "rhs of assignment");
        assert!(!classify("fn t() -> int {\n  return F|\n}"), "return value");
        assert!(!classify("fn foo(n: int) {}\nfn t() {\n  foo(F|)\n}"), "call argument");
        assert!(
            !classify("struct Point { x: int }\nfn t() {\n  let p = Point { x: F| }\n}"),
            "struct-literal field value"
        );
        assert!(!classify("fn t() {\n  let a = [F|]\n}"), "array literal element");
        assert!(!classify("fn t(x: int) {\n  if x == F| {}\n}"), "comparison rhs");
        assert!(!classify("fn t(v: int) {\n  match v { F| }\n}"), "match arm pattern");
        assert!(
            !classify("enum SpellState { Idle }\nfn t() {\n  let s = SpellState::F|\n}"),
            "after scope resolution"
        );
    }
}
