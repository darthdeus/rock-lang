//! Find completion contexts in the AST after inserting a probe at the cursor.

use rock_tokenizer::TokenKind;
use skull_parser::Idx;
use skull_parser::ast::{
    BlockData, CallSyntax, ExprData, ExprKind, IfLetClause, ItemKind, Path, Pattern, StatementKind,
};
use ustr::Ustr;

pub(crate) struct CursorAst {
    ast: skull_parser::Ast,
    /// Span offsets in the AST index into this buffer, not the live one.
    text: String,
    chain: Vec<Idx<ExprData>>,
    /// The live cursor for the insertion munges; the run start for the
    /// token-replacement munge, whose sentinel replaces the whole run.
    probe: usize,
}

/// Insertion yields `1__rock_completion__`, which fails tokenize and drops
/// the statement; replacement yields a clean ident.
fn token_replacement_munge(src: &str, offset: usize) -> Option<(String, usize)> {
    let (s, e) = crate::completion::classify::ident_range_around(src, offset);
    if s == e {
        return None;
    }
    let mut out = String::with_capacity(src.len() + 24);
    out.push_str(&src[..s]);
    out.push_str("__rock_completion__");
    out.push_str(&src[e..]);
    Some((crate::completion::classify::balance_delimiters(out), s))
}

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

/// Locate the UFCS receiver even if its trailing arguments fail to parse.
pub(crate) enum ReceiverAnchor {
    /// `type_at` on its last byte resolves it, surviving a broken trailing arg.
    End(usize),
    /// The receiver is itself a call (`a.b().m(|`): its type is this
    /// callee's unique return type, resolved structurally (no inference).
    ChainCall(String),
}

pub(crate) struct NamedArgSlot {
    pub callee: String,
    pub positional: usize,
    pub named: Vec<String>,
    /// Probe offsets inside each `..spread` operand before the cursor; their
    /// struct fields count as already supplied.
    pub spread_probes: Vec<usize>,
}

pub(crate) enum Record {
    Struct { name: String, brace_open: usize },
    Variant { name: String, variant: String },
}

impl Record {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Struct { name, .. } | Self::Variant { name, .. } => name,
        }
    }
}

type FieldValue = (String, String, bool);

fn enum_path_names(path: &Path) -> Option<(&skull_parser::ast::Ident, &skull_parser::ast::Ident)> {
    let len = path.segments.len();
    (len >= 2).then(|| (&path.segments[len - 2], &path.segments[len - 1]))
}

fn is_paren_call(syntax: &CallSyntax) -> bool {
    matches!(
        syntax,
        CallSyntax::Normal
            | CallSyntax::At
            | CallSyntax::Macro
            | CallSyntax::MethodMacro
            | CallSyntax::AtMacro
            | CallSyntax::AtMethodMacro
            | CallSyntax::TypeArgs
    )
}

impl CursorAst {
    pub(crate) fn at(src: &str, offset: usize) -> Option<CursorAst> {
        let file = Ustr::from("<cursor>");
        let mut candidates: Vec<(String, usize)> =
            crate::completion::classify::completion_munges(src, offset)
                .into_iter()
                .map(|m| (m, offset))
                .collect();
        candidates.extend(token_replacement_munge(src, offset));
        let (head, tail) = src.split_at(offset.min(src.len()));
        // The comma separates a following argument, and the arrow keeps a
        // fresh match arm from being discarded during recovery.
        for suffix in [",", " => {}"] {
            let text = format!("{head}__rock_completion__{suffix}{tail}");
            candidates.push((
                crate::completion::classify::balance_delimiters(text),
                offset,
            ));
        }
        candidates.push((src.to_string(), offset));

        let mut best = None;
        let mut best_rank = u8::MAX;
        for (text, probe) in candidates {
            let Ok(ast) = skull_parser::parse_module(&text, file) else {
                continue;
            };
            let chain = Self::expr_chain(&ast, probe);
            let rank = match chain.as_slice() {
                [] => 3,
                [only] if ast.exprs[*only].span.offset_range().0 >= probe => 2,
                _ if ast.recovery_errors.is_empty() => {
                    return Some(Self {
                        ast,
                        text,
                        probe,
                        chain,
                    });
                }
                [probe, parent, ..]
                    if matches!(
                        &ast.exprs[*parent].kind,
                        ExprKind::FunctionCall { func, .. } if func == probe
                    ) =>
                {
                    1
                }
                _ => 0,
            };
            if rank < best_rank {
                best_rank = rank;
                best = Some(Self {
                    ast,
                    text,
                    probe,
                    chain,
                });
            }
        }
        best
    }

    fn expr(&self, i: Idx<ExprData>) -> &ExprData {
        &self.ast.exprs[i]
    }

    /// Span nesting is ancestry in a well-formed parse, so no parent map is
    /// needed.
    fn expr_chain(ast: &skull_parser::Ast, offset: usize) -> Vec<Idx<ExprData>> {
        let mut chain: Vec<_> = ast
            .exprs
            .iter()
            .filter_map(|(i, expr)| crate::query::span_contains(&expr.span, offset).then_some(i))
            .collect();
        chain.sort_by_key(|&i| {
            let (start, end) = ast.exprs[i].span.offset_range();
            end - start
        });
        chain
    }

    /// A direct yield operand can promote coroutine candidates.
    pub(crate) fn after_yield(&self) -> bool {
        let chain = &self.chain;
        chain.windows(2).any(|w| match self.expr(w[1]).kind {
            ExprKind::Yield(Some(op)) | ExprKind::YieldDelegate(op) => op == w[0],
            _ => false,
        })
    }

    pub(crate) fn qualified_base(&self) -> Option<String> {
        if let Some(node) = self.chain.first().copied() {
            match &self.expr(node).kind {
                ExprKind::Path(path) | ExprKind::PathStructLiteral { path, .. } => {
                    if let Some((base, variant)) = enum_path_names(path)
                        && path.segments.len() == 2
                        && !path.from_root
                    {
                        let (s, e) = variant.span.offset_range();
                        if s <= self.probe && self.probe < e {
                            return Some(base.name.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        self.pattern_qualified_base()
    }

    fn walk_patterns(&self, f: &mut dyn FnMut(&Pattern)) {
        fn walk(p: &Pattern, f: &mut dyn FnMut(&Pattern)) {
            f(p);
            match p {
                Pattern::Tuple(subs) | Pattern::Or(subs) => {
                    for sub in subs {
                        walk(sub, f);
                    }
                }
                Pattern::PathVariant {
                    args: Some(args), ..
                } => {
                    for sub in args {
                        walk(sub, f);
                    }
                }
                Pattern::PathRecord { fields, .. } => {
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
            match &st.kind {
                StatementKind::WhileLet(wl) => walk(&wl.pattern, f),
                StatementKind::Let(l) => walk(&l.pattern, f),
                StatementKind::For(fo) => walk(&fo.pattern, f),
                _ => {}
            }
        }
    }

    /// `Enum::<cursor>` inside a PATTERN (match arm, if/while-let binding).
    fn pattern_qualified_base(&self) -> Option<String> {
        let off = self.probe;
        let mut found = None;
        self.walk_patterns(&mut |p| {
            let path = match p {
                Pattern::PathVariant { path, .. } | Pattern::PathRecord { path, .. } => path,
                _ => return,
            };
            let Some((base, variant)) = enum_path_names(path) else {
                return;
            };
            let (s, e) = variant.span.offset_range();
            if s <= off && off < e {
                found = Some(base.name.to_string());
            }
        });
        found
    }

    /// `Enum::Variant { <cursor> }` in a pattern: `(enum, variant)`.
    fn pattern_record(&self) -> Option<(String, String)> {
        let mut found = None;
        self.walk_patterns(&mut |p| {
            let Pattern::PathRecord { path, .. } = p else {
                return;
            };
            let Some((base, variant)) = enum_path_names(path) else {
                return;
            };
            let ve = variant.span.offset_range().1;
            let Some(brace) = self.text[ve..].find('{').map(|i| ve + i) else {
                return;
            };
            let Some(close) = matching_brace(self.text.as_bytes(), brace) else {
                return;
            };
            if brace < self.probe && self.probe <= close {
                found = Some((base.name.to_string(), variant.name.to_string()));
            }
        });
        found
    }

    /// `struct S { dur: float = 0.|0 }`: `(struct, field, is_lambda_body)`.
    fn decl_field_default(&self) -> Option<(String, String, bool)> {
        for item in &self.ast.items {
            let skull_parser::ast::ItemKind::Struct(sd) = &item.kind else {
                continue;
            };
            for f in &sd.fields {
                let Some(d) = f.default else { continue };
                let v = self.expr(d);
                let (vs, ve) = v.span.offset_range();
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

    fn lambda_body_contains(&self, v: &ExprData) -> bool {
        matches!(&v.kind, ExprKind::Lambda { body, .. }
        if {
            let (bs, be) = self.ast.blocks[*body].span.offset_range();
            bs <= self.probe && self.probe < be
        })
    }

    pub(crate) fn record_contexts(&self) -> (Option<Record>, Option<FieldValue>) {
        self.record_literal().unwrap_or_else(|| {
            let fields = self
                .pattern_record()
                .map(|(name, variant)| Record::Variant { name, variant });
            (fields, self.decl_field_default())
        })
    }

    fn record_literal(&self) -> Option<(Option<Record>, Option<FieldValue>)> {
        for &node in &self.chain {
            let ExprKind::PathStructLiteral { path, fields, .. } = &self.expr(node).kind else {
                continue;
            };
            let name = path.segments.last()?;
            let end = name.span.offset_range().1;
            let Some(brace_open) = self.text[end..].find('{').map(|i| end + i) else {
                continue;
            };
            if self.probe <= brace_open {
                continue;
            }
            if path.segments.len() != 1 || path.from_root {
                let record = enum_path_names(path).map(|(name, variant)| Record::Variant {
                    name: name.name.to_string(),
                    variant: variant.name.to_string(),
                });
                return Some((record, None));
            }
            for (path, value) in fields {
                let expr = self.expr(*value);
                let (start, end) = expr.span.offset_range();
                if start <= self.probe && self.probe < end {
                    if path.len() == 1 && path[0].span.offset_range() == (start, end) {
                        break; // the span of a punned field covers its name
                    }
                    return Some((
                        None,
                        Some((
                            name.name.to_string(),
                            path.last()?.name.to_string(),
                            self.lambda_body_contains(expr),
                        )),
                    ));
                }
            }
            return Some((
                Some(Record::Struct {
                    name: name.name.to_string(),
                    brace_open,
                }),
                None,
            ));
        }
        None
    }

    pub(crate) fn assign_lhs(&self) -> Option<usize> {
        for (_, e) in self.ast.exprs.iter() {
            let ExprKind::Assign { target, value, .. } = &e.kind else {
                continue;
            };
            let (vs, ve) = self.expr(*value).span.offset_range();
            if vs <= self.probe && self.probe <= ve {
                return self.expr(*target).span.offset_range().1.checked_sub(1);
            }
        }
        None
    }

    /// Resolve by name when an assignment has no inferred left-hand type.
    pub(crate) fn assign_target_ident(&self) -> Option<String> {
        for (_, e) in self.ast.exprs.iter() {
            let ExprKind::Assign { target, value, .. } = &e.kind else {
                continue;
            };
            let (vs, ve) = self.expr(*value).span.offset_range();
            if vs <= self.probe && self.probe <= ve {
                return match &self.expr(*target).kind {
                    ExprKind::Ident(id) => Some(id.name.to_string()),
                    _ => None,
                };
            }
        }
        None
    }

    pub(crate) fn comparison_lhs(&self) -> Option<usize> {
        for (_, e) in self.ast.exprs.iter() {
            let ExprKind::BinOp {
                left, op, right, ..
            } = &e.kind
            else {
                continue;
            };
            if !matches!(
                op,
                TokenKind::Equals | TokenKind::NotEquals | TokenKind::DoubleQuestionMark
            ) {
                continue;
            }
            let (vs, ve) = self.expr(*right).span.offset_range();
            if vs <= self.probe && self.probe <= ve {
                return self.expr(*left).span.offset_range().1.checked_sub(1);
            }
        }
        None
    }

    /// `let name: T = <cursor>`: the binding ident's last byte, since the
    /// annotation types the binding even while the init is broken.
    pub(crate) fn typed_let_binding(&self) -> Option<usize> {
        for (_, s) in self.ast.stmts.iter() {
            let StatementKind::Let(l) = &s.kind else {
                continue;
            };
            if l.ty.is_none() {
                continue;
            }
            let Some(v) = l.value else {
                continue;
            };
            let (vs, ve) = self.expr(v).span.offset_range();
            if vs <= self.probe && self.probe <= ve {
                let Pattern::Ident(id) = &l.pattern else {
                    return None;
                };
                return id.span.offset_range().1.checked_sub(1);
            }
        }
        None
    }

    /// False when a lambda body encloses the cursor (its return retargets to
    /// the lambda).
    pub(crate) fn in_return_value(&self) -> bool {
        let in_span = |(vs, ve): (usize, usize)| vs <= self.probe && self.probe <= ve;
        let hit = self.ast.stmts.iter().any(|(_, s)| {
            matches!(&s.kind, StatementKind::Return { value: Some(v), .. }
                if in_span(self.expr(*v).span.offset_range()))
        }) || self.ast.exprs.iter().any(|(_, e)| {
            matches!(&e.kind, ExprKind::ReturnExpr(Some(v))
                if in_span(self.expr(*v).span.offset_range()))
        });
        hit && !self.ast.exprs.iter().any(|(_, e)| {
            matches!(&e.kind, ExprKind::Lambda { .. }) && self.lambda_body_contains(e)
        })
    }

    /// A lambda has its own return target, so exclude its tail from function slots.
    pub(crate) fn in_tail_return(&self) -> bool {
        if self.ast.exprs.iter().any(|(_, e)| {
            matches!(&e.kind, ExprKind::Lambda { .. }) && self.lambda_body_contains(e)
        }) {
            return false;
        }
        let body = self.ast.items.iter().find_map(|it| {
            let ItemKind::Function(f) = &it.kind else {
                return None;
            };
            let b = f.body.rock_block()?;
            let (s, e) = self.ast.blocks[b].span.offset_range();
            (s <= self.probe && self.probe < e).then_some(b)
        });
        body.is_some_and(|b| self.block_tail_has_cursor(b))
    }

    fn block_tail_has_cursor(&self, block: skull_parser::Idx<BlockData>) -> bool {
        let Some(&last) = self.ast.blocks[block].statements.last() else {
            return false;
        };
        let StatementKind::Expression(e) = &self.ast.stmts[last].kind else {
            return false;
        };
        self.expr_tail_has_cursor(*e)
    }

    fn expr_tail_has_cursor(&self, e: skull_parser::Idx<ExprData>) -> bool {
        let (s, t) = self.expr(e).span.offset_range();
        if !(s <= self.probe && self.probe < t) {
            return false;
        }
        match &self.expr(e).kind {
            ExprKind::Match { arms, .. } => arms.iter().any(|a| self.block_tail_has_cursor(a.body)),
            ExprKind::If {
                branches,
                else_branch,
            } => {
                branches.iter().any(|(_, b)| self.block_tail_has_cursor(*b))
                    || else_branch.is_some_and(|b| self.block_tail_has_cursor(b))
            }
            ExprKind::Block(b) => self.block_tail_has_cursor(*b),
            // A cursor nested in an arg or field of the tail is handled earlier in
            // `expected`.
            _ => true,
        }
    }

    pub(crate) fn call_arg(&self) -> Option<(String, u32, Option<usize>)> {
        let (call, _cursor_arg, pos) = self.enclosing_arg_call()?;
        let ExprKind::FunctionCall { func, .. } = &self.expr(call).kind else {
            return None;
        };
        match &self.expr(*func).kind {
            ExprKind::Ident(id) => Some((id.name.to_string(), pos as u32, None)),
            ExprKind::Path(path) => {
                Some((path.segments.last()?.name.to_string(), pos as u32, None))
            }
            ExprKind::FieldAccess(recv, method) | ExprKind::SafeNavigation(recv, method) => {
                let recv_end = self.expr(*recv).span.offset_range().1;
                Some((method.name.to_string(), pos as u32 + 1, Some(recv_end)))
            }
            _ => None,
        }
    }

    fn enclosing_arg_call(&self) -> Option<(Idx<ExprData>, Option<Idx<ExprData>>, usize)> {
        let chain = &self.chain;
        for (ci, &node) in chain.iter().enumerate() {
            if matches!(&self.expr(node).kind, ExprKind::Lambda { .. })
                && !self.lambda_body_contains(self.expr(node))
            {
                return None;
            }
            let ExprKind::FunctionCall {
                func, args, syntax, ..
            } = &self.expr(node).kind
            else {
                continue;
            };
            if !is_paren_call(syntax) {
                continue;
            }
            if ci == 0 {
                // An empty argument list still puts the cursor inside the call node.
                let func_end = self.expr(*func).span.offset_range().1;
                let Some(paren) = self.text[func_end..].find('(').map(|i| func_end + i) else {
                    continue;
                };
                if self.probe <= paren {
                    continue;
                }
                let pos = args
                    .iter()
                    .filter(|a| self.expr(a.value).span.offset_range().1 <= self.probe)
                    .count();
                return Some((node, None, pos));
            } else {
                let inner = chain[ci - 1];
                if *func == inner {
                    continue;
                }
                let Some(pos) = args.iter().position(|a| a.value == inner) else {
                    continue;
                };
                return Some((node, Some(inner), pos));
            }
        }
        None
    }

    pub(crate) fn named_arg_slot(&self) -> Option<NamedArgSlot> {
        let (call, cursor_arg, _pos) = self.enclosing_arg_call()?;
        let ExprKind::FunctionCall { func, args, .. } = &self.expr(call).kind else {
            return None;
        };
        let callee = match &self.expr(*func).kind {
            ExprKind::Ident(id) => id.name.to_string(),
            ExprKind::Path(path) => path.segments.last()?.name.to_string(),
            _ => return None,
        };
        let mut positional = 0usize;
        let mut named: Vec<String> = Vec::new();
        let mut spread_probes: Vec<usize> = Vec::new();
        for a in args.iter() {
            if cursor_arg == Some(a.value) {
                continue;
            }
            // Without a cursor node only args ending before the probe are supplied;
            // with one, every other arg is present.
            if cursor_arg.is_none() && self.expr(a.value).span.offset_range().1 > self.probe {
                continue;
            }
            if a.is_spread {
                let (s, e) = self.expr(a.value).span.offset_range();
                spread_probes.push(e.saturating_sub(1).max(s));
                continue;
            }
            match &a.name {
                Some(n) => named.push(n.name.to_string()),
                None => positional += 1,
            }
        }
        Some(NamedArgSlot {
            callee,
            positional,
            named,
            spread_probes,
        })
    }

    /// Positive evidence only: no containing type node means expression.
    pub(crate) fn is_type_position(&self) -> bool {
        let ty = self
            .ast
            .types
            .iter()
            .map(|(_, t)| t.span.offset_range())
            .filter(|(s, e)| *s <= self.probe && self.probe < *e)
            .min_by_key(|(s, e)| e - s);
        let Some((ts, te)) = ty else { return false };
        let chain = &self.chain;
        if let Some(&inner) = chain.first()
            && chain.iter().skip(1).any(|i| {
                matches!(
                    &self.expr(*i).kind,
                    ExprKind::BinOp { right, op: TokenKind::LessThan, .. } if *right == inner
                )
            })
        {
            // The parser also builds a type-shaped RHS for an unspaced `<` comparison.
            return false;
        }
        match self.chain.first() {
            None => true,
            Some(&e) => {
                // Types nest inside exprs (ascription, casts): the smaller span wins.
                let (es, ee) = self.expr(e).span.offset_range();
                (te - ts) <= (ee - es)
            }
        }
    }

    pub(crate) fn param_type(&self) -> Option<(String, String)> {
        for item in &self.ast.items {
            let skull_parser::ast::ItemKind::Function(fd) = &item.kind else {
                continue;
            };
            for p in &fd.params {
                let (ts, te) = self.ast.types[p.ty].span.offset_range();
                if ts <= self.probe && self.probe < te {
                    return Some((p.name.name.to_string(), fd.name.name.to_string()));
                }
            }
        }
        None
    }

    /// The scrutinee's last byte when the cursor is in a match arm's pattern,
    /// outside any payload or record braces.
    pub(crate) fn match_pattern_scrutinee(&self) -> Option<usize> {
        self.ast.exprs.values().find_map(|e| {
            let ExprKind::Match { scrutinee, arms } = &e.kind else {
                return None;
            };
            let end = self.expr(*scrutinee).span.offset_range().1;
            let mut start = end + self.text.get(end..)?.find('{')? + 1;
            for arm in arms {
                let body = self.ast.blocks[arm.body].span.offset_range();
                let head = self.text.get(start..body.0)?;
                let arrow = start + head.rfind("=>")?;
                if (start..=arrow).contains(&self.probe) {
                    let in_guard = arm.guard.is_some_and(|g| {
                        let (gs, ge) = self.expr(g).span.offset_range();
                        gs <= self.probe && self.probe <= ge
                    });
                    let depth = self.text[start..self.probe]
                        .bytes()
                        .fold(0i32, |d, c| match c {
                            b'(' | b'{' | b'[' => d + 1,
                            b')' | b'}' | b']' => d - 1,
                            _ => d,
                        });
                    return (!in_guard && depth == 0).then(|| end - 1);
                }
                start = body.1;
            }
            None
        })
    }

    pub(crate) fn coroutine_first_generic_arg(&self) -> bool {
        use skull_parser::ast::TypeExprKind;
        self.ast.types.iter().any(|(_, t)| {
            let TypeExprKind::App { ctor, args } = &t.kind else {
                return false;
            };
            if ctor.name.as_str() != "Coroutine" {
                return false;
            }
            args.first().is_some_and(|&a| {
                let (s, e) = self.ast.types[a].span.offset_range();
                s <= self.probe && self.probe < e
            })
        })
    }

    /// The member chain is the whole `if`/`while` condition; `if a &&
    /// thing.foo|` is not.
    pub(crate) fn bool_condition_chain(&self) -> bool {
        let Some(&top) = self.chain.first() else {
            return false;
        };
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
            || self
                .ast
                .stmts
                .iter()
                .any(|(_, st)| matches!(&st.kind, StatementKind::While(w) if w.condition == top))
    }

    pub(crate) fn member_receiver(&self) -> Option<(usize, Option<Vec<String>>)> {
        let &node = self.chain.first()?;
        let (ExprKind::FieldAccess(recv, member) | ExprKind::SafeNavigation(recv, member)) =
            &self.expr(node).kind
        else {
            return None;
        };
        if self.probe < member.span.offset_range().0 {
            return None;
        }
        Some((
            self.expr(*recv).span.offset_range().1,
            self.receiver_path(*recv),
        ))
    }

    fn receiver_path(&self, mut cur: Idx<ExprData>) -> Option<Vec<String>> {
        let mut segs: Vec<String> = Vec::new();
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

    /// Matches `call_arg`'s Some-with-receiver cases exactly.
    pub(crate) fn call_receiver(&self) -> Option<ReceiverAnchor> {
        self.call_arg()?.2?;
        let chain = &self.chain;
        for &node in chain.iter() {
            let ExprKind::FunctionCall { func, syntax, .. } = &self.expr(node).kind else {
                continue;
            };
            if !is_paren_call(syntax) {
                continue;
            }
            let (ExprKind::FieldAccess(recv, _) | ExprKind::SafeNavigation(recv, _)) =
                &self.expr(*func).kind
            else {
                continue;
            };
            let r = self.expr(*recv);
            return Some(match &r.kind {
                ExprKind::FunctionCall {
                    func: rf, syntax, ..
                } if is_paren_call(syntax) => match &self.expr(*rf).kind {
                    ExprKind::Ident(id) => ReceiverAnchor::ChainCall(id.name.to_string()),
                    ExprKind::FieldAccess(_, m) | ExprKind::SafeNavigation(_, m) => {
                        ReceiverAnchor::ChainCall(m.name.to_string())
                    }
                    _ => ReceiverAnchor::End(r.span.offset_range().1),
                },
                _ => ReceiverAnchor::End(r.span.offset_range().1),
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
        // `1__rock_completion__` fails to tokenize; the replacement munge recovers.
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
        let (_, _, cx) = cx(
            "enum E { X, Y }\nfn t(e: E) -> int {\n  match e {\n    E::$0X => 1,\n    _ => 2,\n  }\n}\n",
        );
        assert_eq!(cx.qualified_base(), Some("E".to_string()));
    }

    #[test]
    fn call_arg_survives_a_following_lambda_arg() {
        // The sentinel juxtaposed with the next arg fails to parse; the
        // sentinel-plus-comma munge recovers.
        let (_, _, cx) = cx(
            "fn apply(n: int, f: (int) -> int) -> int { return f(n) }\nfn t() {\n  apply(3, $0(x: int) => x * x)\n}\n",
        );
        assert_eq!(cx.call_arg(), Some(("apply".to_string(), 1, None)));
    }

    #[test]
    fn qualified_base_in_a_path_record_literal() {
        let (_, _, cx) = cx("enum E { B { d: int } }\nfn t() {\n  let v = E::$0B { d: 1 }\n}\n");
        assert_eq!(cx.qualified_base(), Some("E".to_string()));
    }

    #[test]
    fn call_arg_between_named_args() {
        // Every sentinel munge breaks here, so the raw parse wins and the cursor
        // sits between arg nodes.
        let (_, _, cx) =
            cx("fn draw(a: int, b: int, c: int) {}\nfn t() {\n  draw(1, b: 2,$0 c: 3)\n}\n");
        assert_eq!(cx.call_arg(), Some(("draw".to_string(), 2, None)));
    }

    #[test]
    fn call_arg_suppressed_in_a_lambda_param_list() {
        let (_, _, cx) = cx(
            "fn apply(n: int, f: (int) -> int) -> int { return f(n) }\nfn t() {\n  apply(3, (x$0: int) => x * x)\n}\n",
        );
        assert_eq!(cx.call_arg(), None);
    }

    #[test]
    fn call_arg_fires_in_a_lambda_body() {
        let (_, _, cx) = cx(
            "fn apply(n: int, f: (int) -> int) -> int { return f(n) }\nfn t() {\n  apply(3, (x: int) => x$0)\n}\n",
        );
        assert_eq!(cx.call_arg(), Some(("apply".to_string(), 1, None)));
    }

    #[test]
    fn qualified_base_in_a_fresh_match_arm() {
        // Recovery drops an arm with no `=>`; only the arm-completing rung keeps it.
        let (_, _, cx) = cx("enum E { X, Y(int) }\nfn t(e: E) {\n  match e {\n    E::$0\n  }\n}\n");
        assert_eq!(cx.qualified_base(), Some("E".to_string()));
    }

    #[test]
    fn qualified_base_mid_variant_ident_in_record_literal() {
        // Mid-ident, the sentinel glues into one identifier that parses cleanly.
        let (_, _, cx) = cx(
            "enum E { B { d: int, h: int } }\nfn t(h: int) {\n  let v = E::B$0andaging { d: 1, h }\n}\n",
        );
        assert_eq!(cx.qualified_base(), Some("E".to_string()));
    }

    #[test]
    fn enum_record_fires_in_a_record_pattern() {
        let (_, _, cx) = cx(
            "enum E { B { d: int, h: int } }\nfn t(e: E) {\n  match e {\n    E::B { $0d, .. } => {}\n    _ => {}\n  }\n}\n",
        );
        assert!(
            matches!(cx.record_contexts().0, Some(Record::Variant { name, variant }) if name == "E" && variant == "B")
        );
    }

    #[test]
    fn struct_field_value_fires_in_a_decl_field_default() {
        let (_, _, cx) = cx("struct S {\n  dur: float = 0.$00,\n  n: int = 1,\n}\n");
        assert_eq!(
            cx.record_contexts().1,
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
        let (_, _, cx) = cx("fn t() {\n  // Coroutine<x$0\n}\n");
        assert!(!cx.coroutine_first_generic_arg());
    }

    #[test]
    fn bool_condition_chain_true_for_a_whole_condition() {
        let (_, _, cx) = cx("struct T { ok: bool }\nfn t(thing: T) {\n  if thing.o$0\n}\n");
        assert!(cx.bool_condition_chain());
    }

    #[test]
    fn bool_condition_chain_false_under_a_conjunction() {
        let (_, _, cx) =
            cx("struct T { ok: bool }\nfn t(a: bool, thing: T) {\n  if a && thing.o$0\n}\n");
        assert!(!cx.bool_condition_chain());
    }

    #[test]
    fn qualified_base_in_expression_position() {
        let (_, _, cx) = cx("enum E { X }\nfn t() {\n  let v = E::$0X\n}\n");
        assert_eq!(cx.qualified_base(), Some("E".to_string()));
    }
}

#[cfg(test)]
mod tokenize_frontier {
    use super::*;

    #[test]
    fn classifies_the_sampled_fixture_offsets() {
        let src = include_str!("../tests/fixtures/cursor_frontier.rock").repeat(6);
        for offset in (1..src.len()).step_by(5) {
            if src.is_char_boundary(offset) {
                assert!(
                    CursorAst::at(&src, offset).is_some(),
                    "fixture did not classify at byte {offset}"
                );
            }
        }
    }

    /// Raw buffers that fail tokenize must still classify via the balanced
    /// munges.
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
            assert!(
                got.is_some(),
                "must classify (Some) for broken tail: {src:?}"
            );
        }
    }
}

#[cfg(test)]
mod type_position_matrix {
    use super::*;

    fn classify(src: &str) -> bool {
        let at = src.find('|').expect("mark the cursor with `|`");
        let cleaned: String = src[..at].to_string() + &src[at + 1..];
        CursorAst::at(&cleaned, at)
            .expect("classifiable")
            .is_type_position()
    }

    #[test]
    fn type_positions() {
        assert!(classify("fn t() {\n  let x: F|\n}"), "let annotation");
        assert!(classify("fn f(p: F|) {}"), "param type");
        assert!(classify("fn f(a: int, p: F|) {}"), "later param type");
        assert!(classify("fn f() -> F| {}"), "return type");
        assert!(classify("struct S { field: F| }"), "struct field type");
        assert!(classify("global g: F|"), "global annotation");
        assert!(
            classify("fn t() {\n  let v: Vec<F|>\n}"),
            "generic argument"
        );
        assert!(
            classify("fn t() {\n  let m: Map<K, V|>\n}"),
            "later generic argument"
        );
        assert!(
            classify("fn t(y: int) {\n  let x = y as F|\n}"),
            "type cast"
        );
        assert!(classify("fn t() {\n  let a: [F|]\n}"), "array type element");
        assert!(classify("enum E { V(F|) }"), "tuple-variant payload");
    }

    #[test]
    fn expression_positions() {
        assert!(!classify("fn t() {\n  let x = F|\n}"), "rhs of assignment");
        assert!(!classify("fn t() -> int {\n  return F|\n}"), "return value");
        assert!(
            !classify("fn foo(n: int) {}\nfn t() {\n  foo(F|)\n}"),
            "call argument"
        );
        assert!(
            !classify("struct Point { x: int }\nfn t() {\n  let p = Point { x: F| }\n}"),
            "struct-literal field value"
        );
        assert!(
            !classify("fn t() {\n  let a = [F|]\n}"),
            "array literal element"
        );
        assert!(
            !classify("fn t(x: int) {\n  if x == F| {}\n}"),
            "comparison rhs"
        );
        assert!(
            !classify("fn t(v: int) {\n  match v { F| }\n}"),
            "match arm pattern"
        );
        assert!(
            !classify("enum SpellState { Idle }\nfn t() {\n  let s = SpellState::F|\n}"),
            "after scope resolution"
        );
    }
}
