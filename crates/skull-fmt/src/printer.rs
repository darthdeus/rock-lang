//! Walks the raw skull-parser AST into a `Doc` layout.
//!
//! Constructs whose source spelling the raw AST does not fully preserve
//! (inline-lua / extern bodies; `operator +` folded to `fn __op_add`;
//! decorators; compiler directives; macro invocations) are sliced verbatim from
//! source. Everything else is built structurally so width-driven wrapping and
//! spacing are normalized.

use std::collections::HashSet;

use crate::doc::{hardline, line, nil, softline, Doc};
use crate::trivia::Trivia;
use crate::FormatConfig;
use rock_core::Span;
use rock_tokenizer::{TokenKind, Tokenizer};
use skull_parser::ast::*;
use ustr::Ustr;

/// Identifier spelling for output. The tokenizer escapes Lua keywords used as
/// Rock identifiers (`end` is stored as `__rock_end`); undo that so the source
/// name is emitted. A non-keyword name passes through unchanged.
fn kw(name: Ustr) -> String {
    Tokenizer::unescape_lua_keyword(name.as_str()).to_string()
}

pub struct Printer<'a> {
    ast: &'a Ast,
    src: &'a str,
    #[allow(dead_code)]
    cfg: &'a FormatConfig,
    trivia: &'a Trivia,
}

impl<'a> Printer<'a> {
    pub fn new(ast: &'a Ast, src: &'a str, cfg: &'a FormatConfig, trivia: &'a Trivia) -> Self {
        Printer {
            ast,
            src,
            cfg,
            trivia,
        }
    }

    fn slice(&self, span: Span) -> &str {
        let (b0, b1) = span.offset_range;
        self.slice_range(b0, b1)
    }

    fn raw(&self, span: Span) -> Doc {
        Doc::text(self.slice(span).trim_end())
    }

    /// Slice the source by byte-offset range. `Span.offset_range` is byte-based
    /// (the tokenizer's `byte_offset` advances by `len_utf8()` per char), so the
    /// offsets index `&str` directly -- no char->byte remap. `get` guards against
    /// a malformed span (out of bounds / not on a UTF-8 boundary).
    fn slice_range(&self, b0: usize, b1: usize) -> &str {
        let len = self.src.len();
        let (b0, b1) = (b0.min(len), b1.min(len));
        if b0 <= b1 {
            self.src.get(b0..b1).unwrap_or("")
        } else {
            ""
        }
    }

    /// Did the author write a trailing comma after this (last) entry? Scan the
    /// source from the entry's end past whitespace and `//` line comments: the
    /// first significant char is either the author's `,` or the closing
    /// delimiter. ASCII bytes only -- a multibyte char's high bit makes it fall
    /// to the `false` arm, which is correct (any non-`,` significant char means
    /// no trailing comma).
    fn source_trailing_comma(&self, last: Span) -> bool {
        let bytes = self.src.as_bytes();
        let mut i = last.offset_range.1.min(bytes.len());
        while i < bytes.len() {
            match bytes[i] {
                b' ' | b'\t' | b'\r' | b'\n' => i += 1,
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b',' => return true,
                _ => return false,
            }
        }
        false
    }

    // ---- module -----------------------------------------------------------

    pub fn module(&self) -> Doc {
        // The `mod a::b;` file header (v1) prints first, verbatim: it has no
        // internal layout decisions.
        let header = self.ast.module_header.as_ref().map(|h| self.raw(h.span));
        // The `@memory(<class>)` file header prints the same way, and before
        // `mod` when both are present (the parser accepts either order).
        let memory_header = self.ast.memory_header.as_ref().map(|h| self.raw(h.span));
        // Render every item, then preserve hand-aligned runs of adjacent items
        // verbatim (sfx.rock's aligned `global` declarations) before assembling.
        let item_spans: Vec<Span> = self.ast.items.iter().map(|it| it.span).collect();
        let item_eligible: Vec<bool> = self
            .ast
            .items
            .iter()
            .zip(&item_spans)
            .map(|(it, sp)| {
                sp.line_range.0 == sp.line_range.1
                    && it.annotations.is_empty()
                    && !matches!(it.kind, ItemKind::Comment(_))
            })
            .collect();
        // A trailing-comment `Comment` item (re-emitted as a trailing comment on
        // the preceding item) is transparent: it mustn't break an aligned run of
        // globals that carry trailing comments (`global X: T = ..  // note`).
        let item_skip: Vec<bool> = self
            .ast
            .items
            .iter()
            .enumerate()
            .map(|(idx, it)| {
                matches!(it.kind, ItemKind::Comment(_))
                    && idx > 0
                    && self.ast.items[idx - 1].span.line_range.1 == it.span.line_range.0
            })
            .collect();
        let mut item_docs: Vec<Doc> = self.ast.items.iter().map(|it| self.item(it)).collect();
        // Aligned items are sliced verbatim INCLUDING any trailing comment, so the
        // author's `//`-column alignment survives (a re-applied comment would be
        // re-spaced by the trailing-comment aligner).
        for k in self.aligned_indices(&item_spans, &item_eligible, &item_skip) {
            item_docs[k] = self.verbatim_full_line(item_spans[k]);
        }

        let mut parts = Vec::new();
        if let Some(h) = memory_header {
            parts.push(h);
            parts.push(hardline());
            parts.push(hardline());
        }
        if let Some(h) = header {
            // The file header leads the output with one blank line after it.
            parts.push(h);
            parts.push(hardline());
            parts.push(hardline());
        }
        let mut prev_end: Option<usize> = None;
        let mut prev_was_comment = false;
        let mut prev_multiline = false;
        for (idx, item) in self.ast.items.iter().enumerate() {
            // At the top level the parser captures a trailing comment as its own
            // `Comment` item; trivia already re-emits it as a trailing comment on
            // the preceding item, so skip it here to avoid double output.
            if matches!(item.kind, ItemKind::Comment(_))
                && prev_end == Some(item.span.line_range.0)
            {
                continue;
            }
            let item_doc = item_docs[idx].clone();
            let cur_multiline = self.item_forces_blank(item, &item_doc);
            if let Some(prev) = prev_end {
                parts.push(hardline());
                // A declaration that renders as a multi-line `{ .. }` block
                // (struct/enum with members, a multi-line fn body) is always
                // separated from its neighbour by a blank line; runs of one-line
                // decls (globals, operators, inline/extern fns, newtypes) stay
                // adjacent. A leading comment stays attached to its item.
                let source_blanks = self.trivia.blank_count_between(prev, self.item_top_line(item));
                let force = !prev_was_comment && (prev_multiline || cur_multiline);
                let min_blanks = if force { 1 } else { 0 };
                for _ in 0..source_blanks.max(min_blanks) {
                    parts.push(hardline());
                }
            }
            parts.push(item_doc);
            prev_end = Some(item.span.line_range.1);
            prev_was_comment = matches!(item.kind, ItemKind::Comment(_));
            prev_multiline = cur_multiline;
        }
        Doc::concat(parts)
    }

    /// The source line an item begins on, including any annotations (used for
    /// blank-line accounting).
    fn item_top_line(&self, item: &Item) -> usize {
        item.annotations
            .first()
            .map(|a| a.span.line_range.0)
            .unwrap_or(item.span.line_range.0)
    }

    /// The source line the item's own declaration (after annotations) begins on.
    fn item_decl_line(&self, item: &Item) -> usize {
        match &item.kind {
            ItemKind::Function(f) => f.keyword_span.unwrap_or(f.span).line_range.0,
            ItemKind::Global(g) => g.keyword_span.unwrap_or(g.span).line_range.0,
            ItemKind::Struct(s) => s.keyword_span.unwrap_or(s.span).line_range.0,
            ItemKind::Enum(e) => e.keyword_span.unwrap_or(e.span).line_range.0,
            // Newtype has no keyword span; the name shares the `newtype` line.
            ItemKind::Newtype(n) => n.name.span.line_range.0,
            _ => item.span.line_range.0,
        }
    }

    /// Whether `item` is separated from its neighbours by a blank line: it
    /// renders as a multi-line declaration. Struct/enum with members and a
    /// multi-line-bodied Rock fn qualify; globals, newtypes, and inline-lua /
    /// extern fns are one-liner decls even when a verbatim multi-line string body
    /// spans lines, so they stay adjacent.
    fn item_forces_blank(&self, item: &Item, item_doc: &Doc) -> bool {
        match &item.kind {
            // A multi-line `{ .. }` def forces a blank; a single-line one
            // (`enum E { A, B }`) stays adjacent to its neighbours.
            ItemKind::Struct(s) => {
                !s.fields.is_empty() && s.span.line_range.0 != s.span.line_range.1
            }
            ItemKind::Enum(e) => {
                !e.variants.is_empty() && e.span.line_range.0 != e.span.line_range.1
            }
            // inline-lua / extern fns: verbatim one-liner, never forces a blank.
            ItemKind::Function(f) if !matches!(f.body, FunctionBody::Rock(_)) => false,
            // Rock-bodied fns (incl. block-bodied operators): a width-driven
            // struct/if expansion isn't visible in the source span, so decide off
            // the ACTUAL rendered output -- keeping the blank stable across passes.
            ItemKind::Function(_) => {
                crate::doc::render(item_doc, self.cfg.max_line_length, self.cfg.indent_size)
                    .contains('\n')
            }
            _ => false,
        }
    }

    /// Items whose exact source spelling the raw AST cannot reconstruct, sliced
    /// whole from source (`item.span` covers any prefix AND annotations).
    fn is_verbatim_item(&self, item: &Item) -> bool {
        match &item.kind {
            ItemKind::Decorator(_)
            | ItemKind::CompilerDirective(_)
            | ItemKind::MacroInvocation { .. } => true,
            ItemKind::Function(f) => {
                !matches!(f.body, FunctionBody::Rock(_)) || f.name.name.starts_with("__op_")
            }
            _ => false,
        }
    }

    /// A user operator overload, folded by the parser to `fn __op_*` and given a
    /// parser-synthesized `@inline` annotation (`parser.rs`, "rock injects
    /// @inline") whose span is the whole decl and has no source form.
    fn is_operator_item(&self, item: &Item) -> bool {
        matches!(&item.kind, ItemKind::Function(f) if f.name.name.starts_with("__op_"))
    }

    fn item(&self, item: &Item) -> Doc {
        if self.is_operator_item(item) {
            // The synthetic `@inline` (span = whole decl) makes the structural
            // path below print a phantom `@inline` and slice an empty decl after
            // it. `item.span` already covers the real leading annotations, so
            // slice the whole operator verbatim instead.
            let decl = self.slice(item.span).trim();
            let decl = decl.strip_suffix(';').unwrap_or(decl).trim_end();
            return self.with_trailing_comment(Doc::text(decl), item.span, 'N');
        }
        if self.is_verbatim_item(item) {
            // Render annotations structurally (a standalone annotation joins the
            // declaration line) then slice the declaration verbatim, stripping a
            // trailing `;`. The body (inline-lua / extern / operator) isn't
            // recoverable from the AST, so it stays byte-for-byte.
            // An unannotated item's slice starts at `item.span`, which covers
            // its visibility; an annotated one starts after the last
            // annotation, so the visibility is printed ahead of them.
            let mut parts = Vec::new();
            if !item.annotations.is_empty() {
                parts.extend(item_visibility(item.vis.as_ref()));
            }
            for anno in &item.annotations {
                parts.push(self.annotation(anno));
                parts.push(Doc::text(" "));
            }
            let decl_start = item
                .annotations
                .last()
                .map(|a| a.span.offset_range.1)
                .unwrap_or(item.span.offset_range.0);
            let decl = self.slice_range(decl_start, item.span.offset_range.1);
            let decl = decl.trim();
            let decl = decl.strip_suffix(';').unwrap_or(decl).trim_end();
            parts.push(Doc::text(decl));
            return self.with_trailing_comment(Doc::concat(parts), item.span, 'N');
        }
        // Structurally printed declarations re-spell their keyword, so the
        // visibility (which leads the item, annotations included) is printed
        // here. Raw-sliced kinds already carry it in `item.span`.
        let structural = matches!(
            item.kind,
            ItemKind::Function(_) | ItemKind::Global(_) | ItemKind::Struct(_) | ItemKind::Enum(_) | ItemKind::Newtype(_)
        );
        let mut parts = if structural { item_visibility(item.vis.as_ref()) } else { Vec::new() };
        let decl_line = self.item_decl_line(item);
        let annos = &item.annotations;
        for (i, anno) in annos.iter().enumerate() {
            parts.push(self.with_trailing_comment(self.annotation(anno), anno.span, 'N'));
            // Keep the annotation inline iff source had it on the same line as
            // whatever follows (next annotation or the declaration).
            let next_line = annos
                .get(i + 1)
                .map(|a| a.span.line_range.0)
                .unwrap_or(decl_line);
            if anno.span.line_range.0 == next_line {
                parts.push(Doc::text(" "));
            } else {
                parts.push(hardline());
                // Standalone comments between annotation lines (the parser
                // attaches none of them to the item, so recover from trivia).
                for (_, t) in self.trivia.standalone_comments_between(anno.span.line_range.1, next_line) {
                    parts.push(self.comment_line(t));
                    parts.push(hardline());
                }
            }
        }
        // Inline annotations (`@pure fn ..`) sit on the signature line; their
        // width is where the signature actually starts, so the params-wrap
        // decision must account for it. It's the width of the last rendered
        // annotation line (0 when every annotation is on its own line).
        let head_col = crate::doc::render(
            &Doc::concat(parts.clone()),
            1_000_000,
            self.cfg.indent_size,
        )
        .rsplit('\n')
        .next()
        .map_or(0, |l| l.chars().count());
        let body = match &item.kind {
            ItemKind::Function(f) => self.function(f, head_col),
            ItemKind::Global(g) => self.global(g),
            ItemKind::Struct(s) => self.struct_def(s),
            ItemKind::Enum(e) => self.enum_def(e),
            ItemKind::Newtype(n) => self.newtype_def(n),
            ItemKind::Comment(text) => self.comment_node(item.span, text),
            // Constructs whose source spelling the raw AST does not fully
            // recover -- slice verbatim for now.
            ItemKind::Decorator(_)
            | ItemKind::CompilerDirective(_)
            | ItemKind::MacroInvocation { .. }
            // `use` / inline `mod` print verbatim: the path/tree spelling is
            // the source spelling (no internal layout decisions to re-flow).
            | ItemKind::Use(_)
            | ItemKind::Module(_) => self.raw(item.span),
        };
        parts.push(body);
        let mut doc = Doc::concat(parts);
        doc = self.with_trailing_comment(doc, item.span, 'N');
        doc
    }

    /// Annotations in a NON-ITEM position (a fn parameter, a struct or
    /// record-variant field): they sit inline ahead of the thing they mark,
    /// one space apart, rather than on their own lines the way an item's do.
    /// `AnnoPos` (`crates/bone-core/src/annotations.rs`) is the full list of
    /// positions the parser accepts.
    fn leading_annotations(&self, annos: &[Annotation]) -> Vec<Doc> {
        annos
            .iter()
            .flat_map(|a| [self.annotation(a), Doc::text(" ")])
            .collect()
    }

    fn annotation(&self, anno: &Annotation) -> Doc {
        if anno.args.is_empty() {
            Doc::text(format!("@{}", kw(anno.name.name)))
        } else {
            // Preserve the author's exact annotation form -- `@late_draw(stage=10)`
            // stays as written, NOT normalized to `stage = 10`.
            self.raw(anno.span)
        }
    }

    // ---- items ------------------------------------------------------------

    fn function(&self, f: &FunctionDef, head_col: usize) -> Doc {
        // Non-Rock bodies (inline-lua / extern / ffi) and operator defs are
        // sliced verbatim -- their source form isn't recoverable from the AST.
        let is_operator = f.name.name.starts_with("__op_");
        if !matches!(f.body, FunctionBody::Rock(_)) || is_operator {
            return self.raw(f.span);
        }

        // A block comment in the signature (`fn f(a, /* c */ b) -> /* c */ T`)
        // has no structural home -- slice the signature verbatim (keeping the
        // body structural). The body's `{` re-emitted by `block`.
        let block = f.body.rock_block().unwrap();
        let sig_start = f.keyword_span.unwrap_or(f.span);
        let body_open = self.ast.b(&block).span.offset_range.0;
        if sig_start.line_range.0 == self.ast.b(&block).span.line_range.0
            && self.trivia.has_block_comment_in(sig_start.offset_range.0, body_open)
        {
            let sig = self.slice_range(sig_start.offset_range.0, body_open);
            return Doc::concat([
                Doc::text(sig.trim_end().to_string()),
                Doc::text(" "),
                self.block(block),
            ]);
        }

        let keyword = if f.is_macro { "macro" } else { "fn" };
        let coroutine = if f.is_coroutine { "co " } else { "" };
        let mut sig = vec![Doc::text(format!("{coroutine}{keyword} "))];
        // D-form: reprint `Type<path_gen>.name<method_gen>(recv, ..)` from the
        // surface split point; the receiver param prints bare (its type lives in
        // the path). `generics` is the merged list; `k` splits path vs method.
        if let Some((ty_ident, k)) = &f.receiver_surface {
            let k = *k;
            sig.push(Doc::text(kw(ty_ident.name)));
            sig.push(self.generics(&f.generics[..k]));
            sig.push(Doc::text("."));
            sig.push(Doc::text(kw(f.name.name)));
            sig.push(self.generics(&f.generics[k..]));
            sig.push(self.params_impl(&f.params, true));
        } else {
            sig.push(Doc::text(kw(f.name.name)));
            sig.push(self.generics(&f.generics));
            sig.push(self.params(&f.params));
        }
        if let Some(ret) = f.return_ty {
            sig.push(Doc::text(" -> "));
            sig.push(self.type_expr(ret));
        }
        // Freeze the signature layout by rendering it standalone (fns are always
        // top-level, so indent 0). The params then wrap only when the SIGNATURE
        // itself overflows -- not because a wide single-line body shares the
        // line during the fit check. The body's inline-vs-expanded form differs
        // pass-to-pass, so measuring params against it made the params oscillate.
        // Measure WITH the trailing ` {` (the opening brace shares the signature
        // line), then strip it back off -- the body block re-emits the `{`.
        sig.push(Doc::text(" {"));
        let sig_brace = crate::doc::render(
            &Doc::concat(sig),
            self.cfg.max_line_length.saturating_sub(head_col),
            self.cfg.indent_size,
        );
        let sig_str = sig_brace.strip_suffix(" {").unwrap_or(&sig_brace);
        Doc::concat([
            Doc::text(sig_str.to_string()),
            Doc::text(" "),
            self.block(block),
        ])
    }

    fn global(&self, g: &GlobalDef) -> Doc {
        let mut parts = Vec::new();
        parts.push(Doc::text(if g.is_const { "const " } else { "global " }));
        parts.push(Doc::text(kw(g.name.name)));
        if let Some(ty) = g.ty {
            parts.push(Doc::text(": "));
            parts.push(self.type_expr(ty));
        }
        if let Some(v) = g.value {
            parts.push(Doc::text(" = "));
            parts.push(self.expr(v));
        }
        Doc::concat(parts)
    }

    fn struct_def(&self, s: &StructDef) -> Doc {
        let prefix = if s.is_nobox {
            "nobox value "
        } else if s.is_value {
            "value "
        } else {
            "struct "
        };
        let head = Doc::concat([
            Doc::text(prefix),
            Doc::text(kw(s.name.name)),
            self.generics(&s.generics),
            Doc::text(" "),
        ]);
        if s.fields.is_empty() {
            return Doc::concat([head, Doc::text("{}")]);
        }
        // Preserve the author's layout: a definition on one source line stays
        // inline (`struct Pt { x: int, y: int }`); multi-line expands.
        if s.span.line_range.0 == s.span.line_range.1 {
            let inner = Doc::join(Doc::text(", "), s.fields.iter().map(|f| self.field(f)));
            return Doc::concat([head, Doc::text("{ "), inner, Doc::text(" }")]);
        }
        let fields = s
            .fields
            .iter()
            .map(|field| (self.field(field), field.span.line_range.0, field.span))
            .collect();
        let span = (s.span.line_range.0, s.span.line_range.1);
        Doc::concat([head, self.comma_block(fields, Some(span), "{", "}")])
    }

    fn field(&self, field: &Field) -> Doc {
        let mut parts = vec![];
        parts.extend(self.leading_annotations(&field.annotations));
        parts.extend(item_visibility(field.vis.as_ref()));
        if field.set_private {
            parts.push(Doc::text("private(set) "));
        }
        if field.is_embed {
            parts.push(Doc::text("embed "));
        }
        parts.extend([
            Doc::text(kw(field.name.name)),
            Doc::text(": "),
            self.type_expr(field.ty),
        ]);
        if let Some(d) = field.default {
            parts.push(Doc::text(" = "));
            parts.push(self.expr(d));
        }
        Doc::concat(parts)
    }

    fn enum_def(&self, e: &EnumDef) -> Doc {
        let head = Doc::concat([
            Doc::text("enum "),
            Doc::text(kw(e.name.name)),
            self.generics(&e.generics),
            Doc::text(" "),
        ]);
        if e.variants.is_empty() {
            return Doc::concat([head, Doc::text("{}")]);
        }
        if e.span.line_range.0 == e.span.line_range.1 {
            let inner = Doc::join(Doc::text(", "), e.variants.iter().map(|v| self.variant(v)));
            return Doc::concat([head, Doc::text("{ "), inner, Doc::text(" }")]);
        }
        let variants = e
            .variants
            .iter()
            .map(|v| (self.variant(v), v.span.line_range.0, v.span))
            .collect();
        let span = (e.span.line_range.0, e.span.line_range.1);
        Doc::concat([head, self.comma_block(variants, Some(span), "{", "}")])
    }

    fn variant(&self, v: &EnumVariant) -> Doc {
        // A block comment among tuple/record fields (`Rect(float, /* w */ float)`)
        // sits between field spans, not inside one -- slice the variant verbatim.
        self.maybe_verbatim_line(v.span, || self.variant_structural(v))
    }

    fn variant_structural(&self, v: &EnumVariant) -> Doc {
        let name = Doc::text(kw(v.name.name));
        match &v.kind {
            EnumVariantKind::Unit => name,
            EnumVariantKind::Tuple(tys) => {
                let inner = Doc::join(Doc::text(", "), tys.iter().map(|t| self.type_expr(*t)));
                Doc::concat([name, Doc::text("("), inner, Doc::text(")")])
            }
            EnumVariantKind::Record(fields) => {
                // A record variant the author wrote on one source line stays
                // inline (`Bandage { duration: float, heal_amount: float }`);
                // multi-line expands. Same preserve rule as struct defs.
                if v.span.line_range.0 == v.span.line_range.1 {
                    let inner =
                        Doc::join(Doc::text(", "), fields.iter().map(|f| self.field(f)));
                    return Doc::concat([name, Doc::text(" { "), inner, Doc::text(" }")]);
                }
                let fs = fields
                    .iter()
                    .map(|f| (self.field(f), f.span.line_range.0, f.span))
                    .collect();
                let span = (v.span.line_range.0, v.span.line_range.1);
                Doc::concat([name, Doc::text(" "), self.comma_block(fs, Some(span), "{", "}")])
            }
        }
    }

    fn newtype_def(&self, n: &NewtypeDef) -> Doc {
        Doc::concat([
            Doc::text("newtype "),
            Doc::text(kw(n.name.name)),
            self.generics(&n.generics),
            Doc::text("("),
            self.type_expr(n.wrapped),
            Doc::text(")"),
        ])
    }

    fn comment_line(&self, text: &str) -> Doc {
        Doc::text(format!("//{text}"))
    }

    /// If `span` is a single source line that carries a block comment the
    /// structural render would drop, slice it verbatim -- block comments are a
    /// preserve-verbatim trigger with no structural home in headers/types/arms.
    /// Multi-line spans keep structural rendering (verbatim would un-format a
    /// nested body).
    fn maybe_verbatim_line(&self, span: Span, fallback: impl FnOnce() -> Doc) -> Doc {
        if span.line_range.0 == span.line_range.1
            && self
                .trivia
                .has_block_comment_in(span.offset_range.0, span.offset_range.1)
        {
            Doc::text(self.slice(span).trim_end().to_string())
        } else {
            fallback()
        }
    }

    /// Prepend an inline block comment the parser dropped from just before a
    /// node (`/* second */ 2`, `/* y field */ y: 2`). Only a SAME-LINE leading
    /// comment (gap is spaces/tabs, no newline) is inlined; an own-line block
    /// comment is left for gap recovery so it stays on its own line.
    fn prepend_leading_block(&self, doc: Doc, node_start: usize) -> Doc {
        if let Some((cstart, cend)) = self.trivia.block_comment_ending_before(node_start) {
            let gap = self.slice_range(cend, node_start);
            if !gap.is_empty() && gap.bytes().all(|b| b == b' ' || b == b'\t') {
                let comment = self.slice_range(cstart, cend);
                return Doc::concat([Doc::text(format!("{comment} ")), doc]);
            }
        }
        doc
    }

    /// A `Comment` AST node (statement / item). The parser stores the inner text
    /// only, losing whether it was `//` or `/* */`; slice the node's span to
    /// preserve a block comment verbatim (`/* between */`, not `// between`).
    /// `//` comments keep the reconstructed form (no source-whitespace churn).
    fn comment_node(&self, span: Span, text: &str) -> Doc {
        let src = self.slice(span);
        if src.trim_start().starts_with("/*") {
            Doc::text(src.trim_end().to_string())
        } else {
            self.comment_line(text)
        }
    }

    // ---- signatures -------------------------------------------------------

    fn generics(&self, generics: &[Generic]) -> Doc {
        if generics.is_empty() {
            return nil();
        }
        let items = generics.iter().map(|g| {
            let mut parts = vec![Doc::text(kw(g.name.name))];
            if let Some(b) = g.bound {
                parts.push(Doc::text(": "));
                parts.push(self.type_expr(b));
            }
            if let Some(d) = g.default {
                parts.push(Doc::text(" = "));
                parts.push(self.type_expr(d));
            }
            Doc::concat(parts)
        });
        Doc::concat([Doc::text("<"), Doc::join(Doc::text(", "), items), Doc::text(">")])
    }

    fn params(&self, params: &[Param]) -> Doc {
        self.params_impl(params, false)
    }

    /// `bare_first` (D-form) prints param 0 as just its name -- the receiver's
    /// type comes from the path in the signature, not the param annotation.
    fn params_impl(&self, params: &[Param], bare_first: bool) -> Doc {
        if params.is_empty() {
            return Doc::text("()");
        }
        let param_doc = |p: &Param, bare: bool| {
            let mut parts = self.leading_annotations(&p.annotations);
            parts.push(Doc::text(kw(p.name.name)));
            if !bare {
                parts.push(Doc::text(": "));
                parts.push(self.type_expr(p.ty));
                if p.is_variadic {
                    parts.push(Doc::text("..."));
                }
                if let Some(d) = p.default {
                    parts.push(Doc::text(" = "));
                    parts.push(self.expr(d));
                }
            }
            Doc::concat(parts)
        };
        // A param trailing comment (`ox: float,  // offset x`) or a standalone
        // comment between parameters forces the comment-preserving multi-line
        // layout. The parser drops the standalone form, so recover it from the
        // source interval spanned by the parameter list.
        let first_line = params.first().unwrap().span.line_range.0;
        let last_line = params.last().unwrap().span.line_range.1;
        let has_standalone_comments = !self
            .trivia
            .standalone_comments_between(first_line.saturating_sub(1), last_line)
            .is_empty();
        if has_standalone_comments
            || params
            .iter()
            .any(|p| self.claimed_trailing_comment(p.span).is_some())
        {
            let entries = params
                .iter()
                .enumerate()
                .map(|(i, p)| (param_doc(p, bare_first && i == 0), p.span.line_range.0, p.span))
                .collect();
            let body_span = has_standalone_comments
                .then_some((first_line.saturating_sub(1), last_line + 1));
            return self.comma_block(entries, body_span, "(", ")");
        }
        let items = params
            .iter()
            .enumerate()
            .map(|(i, p)| param_doc(p, bare_first && i == 0));
        // Params wrap one-per-line when too wide, but take NO trailing comma.
        Doc::group(Doc::concat([
            Doc::text("("),
            Doc::indent(Doc::concat([
                softline(),
                Doc::join(Doc::concat([Doc::text(","), line()]), items),
            ])),
            softline(),
            Doc::text(")"),
        ]))
    }

    // ---- blocks -----------------------------------------------------------

    /// A `{ ... }` block.
    ///
    /// `Collapse` (if/else) collapses a single simple statement onto one line
    /// whenever it fits, regardless of how the source was written. `Preserve`
    /// (fn/lambda bodies) only stays inline if the source already was. `Multi`
    /// (for/while/match arms) never collapses.
    /// fn / lambda / `@` / bare-block bodies. A single simple statement the
    /// author wrote on one source line stays inline when it fits the width (the
    /// group collapses); a wide or multi-statement or control-flow body expands.
    /// Distinct from `flow_block` -- a definition body is canonically multi-line,
    /// so a degenerate one-liner `fn f(){...}` is cleaned up to the block form.
    fn block(&self, block_idx: Idx<BlockData>) -> Doc {
        let block = self.ast.b(&block_idx);
        let stmts = &block.statements;
        if stmts.is_empty() {
            return Doc::concat([Doc::text("{"), hardline(), Doc::text("}")]);
        }
        let source_single_line = block.span.line_range.0 == block.span.line_range.1;
        let stmt_has_comment = stmts.len() == 1
            && self
                .claimed_trailing_comment(self.ast.s(&stmts[0]).span)
                .is_some();
        let collapsible =
            stmts.len() == 1 && !self.stmt_forces_multiline(stmts[0]) && !stmt_has_comment;
        if collapsible && source_single_line {
            let s = self.stmt(stmts[0]);
            return Doc::group(Doc::concat([
                Doc::text("{"),
                Doc::indent(Doc::concat([line(), s])),
                line(),
                Doc::text("}"),
            ]));
        }
        Doc::concat([
            Doc::text("{"),
            Doc::indent(Doc::concat([hardline(), self.stmt_seq(stmts)])),
            hardline(),
            Doc::text("}"),
        ])
    }

    /// Control-flow bodies (for / while / loop / while-let) and match arms.
    /// PRESERVES the author's layout: a body written on one source line stays
    /// inline (`while c { a; b }`, control-flow and multiple statements
    /// included), a multi-line one expands. Unlike `block`, there is no
    /// width-driven collapse -- the author's line breaks are authoritative.
    fn flow_block(&self, block_idx: Idx<BlockData>) -> Doc {
        if let Some(d) = self.inline_single_line_block(block_idx) {
            return d;
        }
        let block = self.ast.b(&block_idx);
        let stmts = &block.statements;
        if stmts.is_empty() {
            // An empty body the author wrote inline (`_ => {}`) stays inline.
            return if block.span.line_range.0 == block.span.line_range.1 {
                Doc::text("{}")
            } else {
                Doc::concat([Doc::text("{"), hardline(), Doc::text("}")])
            };
        }
        Doc::concat([
            Doc::text("{"),
            Doc::indent(Doc::concat([hardline(), self.stmt_seq(stmts)])),
            hardline(),
            Doc::text("}"),
        ])
    }

    /// A block the author wrote entirely on one source line, rendered inline as
    /// `{ stmt; stmt }` (any statement count). `None` when the block spans
    /// multiple source lines, is empty, or any statement carries a trailing
    /// comment / is a `//` comment (a `//` would bury the rest of the line).
    fn inline_single_line_block(&self, block_idx: Idx<BlockData>) -> Option<Doc> {
        let block = self.ast.b(&block_idx);
        let stmts = &block.statements;
        if stmts.is_empty() || block.span.line_range.0 != block.span.line_range.1 {
            return None;
        }
        if stmts.iter().any(|s| {
            matches!(self.ast.s(s).kind, StatementKind::Comment(_))
                || self.claimed_trailing_comment(self.ast.s(s).span).is_some()
        }) {
            return None;
        }
        let inner = Doc::join(Doc::text("; "), stmts.iter().map(|s| self.stmt(*s)));
        Some(Doc::concat([Doc::text("{ "), inner, Doc::text(" }")]))
    }

    fn stmt_seq(&self, stmts: &[Idx<StatementData>]) -> Doc {
        let spans: Vec<Span> = stmts.iter().map(|s| self.ast.s(s).span).collect();
        let is_comment = |i: usize| matches!(self.ast.s(&stmts[i]).kind, StatementKind::Comment(_));

        // Group consecutive statements the author put on ONE source line
        // (`a; b; c`) so they stay on that line -- a Comment statement stands
        // alone. Only when the block spans multiple lines: a whole body minified
        // onto one line (`fn f(){a;b;c}`) is still expanded one-per-line.
        let multi_line_block = stmts
            .first()
            .zip(stmts.last())
            .is_some_and(|(f, l)| self.ast.s(f).span.line_range.0 != self.ast.s(l).span.line_range.1);
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for i in 0..stmts.len() {
            let merge = multi_line_block
                && groups.last().is_some_and(|g| {
                    let p = *g.last().unwrap();
                    !is_comment(p)
                        && !is_comment(i)
                        && spans[p].line_range.0 == spans[i].line_range.0
                });
            if merge {
                groups.last_mut().unwrap().push(i);
            } else {
                groups.push(vec![i]);
            }
        }
        // Per-group span (first statement start .. last statement end).
        let gspans: Vec<Span> = groups
            .iter()
            .map(|g| {
                let (f, l) = (spans[g[0]], spans[*g.last().unwrap()]);
                Span {
                    line_range: (f.line_range.0, l.line_range.1),
                    offset_range: (f.offset_range.0, l.offset_range.1),
                    ..f
                }
            })
            .collect();
        // A multi-statement line is sliced verbatim (preserving the `;`
        // separators and any column alignment); a lone statement renders
        // structurally.
        let mut docs: Vec<Doc> = groups
            .iter()
            .zip(&gspans)
            .map(|(g, gs)| {
                if g.len() > 1 {
                    self.verbatim_full_line(*gs)
                } else {
                    self.stmt(stmts[g[0]])
                }
            })
            .collect();
        // Preserve hand-aligned runs of lone statements verbatim (incl. trailing
        // comment). Multi-statement groups are already verbatim.
        let eligible: Vec<bool> = groups
            .iter()
            .zip(&gspans)
            .map(|(g, gs)| g.len() == 1 && gs.line_range.0 == gs.line_range.1 && !is_comment(g[0]))
            .collect();
        let no_skip = vec![false; groups.len()];
        let mut verbatim = groups.iter().map(|g| g.len() > 1).collect::<Vec<_>>();
        for k in self.aligned_indices(&gspans, &eligible, &no_skip) {
            docs[k] = self.verbatim_full_line(gspans[k]);
            verbatim[k] = true;
        }

        let mut parts = Vec::new();
        for (gi, g) in groups.iter().enumerate() {
            if gi > 0 {
                parts.push(hardline());
                let blanks = self
                    .trivia
                    .blank_count_between(gspans[gi - 1].line_range.1, gspans[gi].line_range.0);
                for _ in 0..blanks {
                    parts.push(hardline());
                }
            }
            let d = docs[gi].clone();
            // A verbatim group already carries its own comment; a `Comment`
            // statement IS one. Otherwise attach the statement's trailing comment.
            if verbatim[gi] || is_comment(g[0]) {
                parts.push(d);
            } else {
                parts.push(self.with_trailing_comment(d, spans[g[0]], 'P'));
            }
        }
        Doc::concat(parts)
    }

    /// A brace-delimited, always-multi-line block of comma-separated entries
    /// (struct fields, enum variants, match arms). Each entry carries its source
    /// `(start_line, end_line)`; the comma is placed *before* any trailing
    /// comment, and blank-line groups between entries are preserved.
    /// The last entry gets a trailing comma iff the author wrote one (preserve,
    /// not normalize -- the comma is cosmetic in the source-driven layout world).
    /// When `body_span` is `Some((open, close))` -- the lines of the enclosing
    /// `{` and `}` -- standalone `//` comments the parser dropped inside the body
    /// are recovered by position; `None` disables recovery (struct literals
    /// deliberately drop interior comments, matching the corpus).
    fn comma_block(
        &self,
        entries: Vec<(Doc, usize, Span)>,
        body_span: Option<(usize, usize)>,
        open: &str,
        close: &str,
    ) -> Doc {
        let mut body = vec![hardline()];
        let mut prev_end: Option<usize> = None;
        let mut last_off: Option<usize> = None;
        for (doc, start, espan) in entries.into_iter() {
            let end = espan.line_range.1;
            last_off = Some(espan.offset_range.1);
            let gap_lo = prev_end.or(body_span.map(|(open, _)| open));
            // Standalone comments authored before this entry, in source-line
            // order: `//` own-line comments AND own-line `/* */` block comments
            // (rendered verbatim so the block form survives).
            let comments: Vec<(usize, Doc)> = match (body_span, gap_lo) {
                (Some(_), Some(lo)) => {
                    let mut v: Vec<(usize, Doc)> = self
                        .trivia
                        .standalone_comments_between(lo, start)
                        .into_iter()
                        .map(|(l, t)| (l, self.comment_line(t)))
                        .collect();
                    for (s, e, l) in self.trivia.own_line_block_comments_between(lo, start) {
                        v.push((l, Doc::text(self.slice_range(s, e).to_string())));
                    }
                    v.sort_by_key(|(l, _)| *l);
                    v
                }
                _ => Vec::new(),
            };
            if prev_end.is_some() {
                body.push(hardline());
            }
            // Preserve the author's layout in the gap: blank lines (only BETWEEN
            // entries, never right after the opening `{`) then the standalone
            // comment block, in source order -- the comment does not absorb the
            // blank that precedes it.
            if let Some(prev) = prev_end {
                let upto = comments.first().map_or(start, |(l, _)| *l);
                for _ in 0..self.trivia.blank_count_between(prev, upto) {
                    body.push(hardline());
                }
            }
            for (_, d) in &comments {
                body.push(d.clone());
                body.push(hardline());
            }
            if let (Some((cline, _)), Some(_)) = (comments.last(), prev_end) {
                for _ in 0..self.trivia.blank_count_between(*cline, start) {
                    body.push(hardline());
                }
            }
            body.push(self.prepend_leading_block(doc, espan.offset_range.0));
            // Each entry's separator comma is source-derived: for constructs
            // where it's syntactically required (struct/array/enum) the source
            // always has it, so this matches forcing it; for match arms (where
            // the separator is optional) it preserves the author's choice.
            if self.source_trailing_comma(espan) {
                body.push(Doc::text(","));
            }
            // Only the entry the comment actually trails claims it: with several
            // entries on one source line (`0, 1, 2,  // Front`) the whitespace-
            // between check leaves it to the last one.
            if let Some((text, col)) = self.claimed_trailing_comment(espan) {
                body.push(self.sentinel(text, col, 'A'));
            }
            prev_end = Some(end);
        }
        // A trailing INLINE block comment on the last entry (`3 /* end */`),
        // between the entry and the close -- appended on the entry's line (the
        // gap check bounds it: a comment in a later construct has non-whitespace
        // -- the close delimiter -- before it and is rejected).
        if let Some(off) = last_off {
            if let Some((s, e)) = self.trivia.inline_block_starting_after(off) {
                let gap = self.slice_range(off, s);
                if gap.bytes().all(|b| b == b' ' || b == b'\t' || b == b',') {
                    body.push(Doc::text(format!(" {}", self.slice_range(s, e))));
                }
            }
        }
        // Trailing standalone comments between the last entry and the `}` --
        // `//` own-line and own-line `/* */`, in source-line order.
        if let (Some((_, close)), Some(prev)) = (body_span, prev_end) {
            let mut tail: Vec<(usize, Doc)> = self
                .trivia
                .standalone_comments_between(prev, close)
                .into_iter()
                .map(|(l, t)| (l, self.comment_line(t)))
                .collect();
            for (s, e, l) in self.trivia.own_line_block_comments_between(prev, close) {
                tail.push((l, Doc::text(self.slice_range(s, e).to_string())));
            }
            tail.sort_by_key(|(l, _)| *l);
            for (_, d) in tail {
                body.push(hardline());
                body.push(d);
            }
        }
        Doc::concat([
            Doc::text(open.to_string()),
            Doc::indent(Doc::concat(body)),
            hardline(),
            Doc::text(close.to_string()),
        ])
    }

    /// A multi-line bracketed construct rendered by SOURCE LINE rather than
    /// one-element-per-line: elements that shared a source line are sliced
    /// VERBATIM together, preserving the author's intra-row column alignment and
    /// field packing (the formatter is a layout-preserver here, not a
    /// normalizer -- see `docs/skull-fmt-plan.md`). An element
    /// that is itself multi-line recurses structurally (its pre-rendered `doc`)
    /// so nested levels re-indent correctly. Trailing comma: non-last rows
    /// always; last row source-derived. No interior-comment handling -- callers
    /// route comment-bearing constructs through `comma_block` instead.
    fn verbatim_rows(&self, rows: Vec<(Span, Doc)>, open: &str, close: &str) -> Doc {
        let last_span = rows.last().map(|(s, _)| *s);
        // Partition into groups of consecutive elements sharing a start line.
        let mut groups: Vec<Vec<(Span, Doc)>> = Vec::new();
        for (sp, doc) in rows {
            match groups.last_mut() {
                Some(g) if g[0].0.line_range.0 == sp.line_range.0 => g.push((sp, doc)),
                _ => groups.push(vec![(sp, doc)]),
            }
        }
        let g_count = groups.len();
        let mut body = vec![hardline()];
        let mut prev_end: Option<usize> = None;
        for (gi, group) in groups.into_iter().enumerate() {
            if let Some(prev) = prev_end {
                body.push(hardline());
                for _ in 0..self.trivia.blank_count_between(prev, group[0].0.line_range.0) {
                    body.push(hardline());
                }
            }
            // A lone element spanning >1 line recurses structurally; otherwise
            // the whole source line (one or more elements) is sliced verbatim.
            if group.len() == 1 && group[0].0.line_range.0 != group[0].0.line_range.1 {
                body.push(group[0].1.clone());
            } else {
                let lo = group.first().unwrap().0.offset_range.0;
                let hi = group.last().unwrap().0.offset_range.1;
                body.push(Doc::text(self.slice_range(lo, hi).trim_end().to_string()));
            }
            let is_last = gi + 1 == g_count;
            if !is_last || last_span.is_some_and(|s| self.source_trailing_comma(s)) {
                body.push(Doc::text(","));
            }
            prev_end = Some(group.last().unwrap().0.line_range.1);
        }
        Doc::concat([
            Doc::text(open.to_string()),
            Doc::indent(Doc::concat(body)),
            hardline(),
            Doc::text(close.to_string()),
        ])
    }

    /// The byte offset of the first non-whitespace character on the source line
    /// containing `off` -- i.e. the line's first token, skipping leading indent.
    fn line_first_token_offset(&self, off: usize) -> usize {
        let b = self.src.as_bytes();
        let mut i = off.min(b.len());
        while i > 0 && b[i - 1] != b'\n' {
            i -= 1;
        }
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
            i += 1;
        }
        i
    }

    /// Token-start columns of a source line and the subset reached via 2+ spaces
    /// of padding (an alignment point the author inserted). Spaces inside strings
    /// or after a `//` comment don't count. Columns are relative to the line's
    /// first token, which is shared across siblings at the same indent -- so two
    /// lines align iff they share a token-start column padded on at least one side.
    fn aligned_columns(&self, line: &str) -> (HashSet<usize>, HashSet<usize>) {
        let b = line.as_bytes();
        let n = b.len();
        // `bounds` = every token boundary column (start + end); `padded` = the
        // boundaries of tokens reached via 2+ spaces of padding. Recording BOTH
        // ends catches left-aligned columns (shared token START, `: Color`) AND
        // right-aligned ones (shared token END, decimal-aligned `180.0` / `80.0`).
        let (mut bounds, mut padded) = (HashSet::new(), HashSet::new());
        let mut i = 0;
        while i < n {
            let gap_start = i;
            while i < n && b[i] == b' ' {
                i += 1;
            }
            if i >= n {
                break;
            }
            if b[i] == b'/' && i + 1 < n && b[i + 1] == b'/' {
                break;
            }
            let gap = i - gap_start;
            let tok_start = i;
            // Consume the token: a maximal run of non-space chars, with string
            // literals (which may contain spaces) treated as part of the token.
            let mut in_str = false;
            let mut q = 0u8;
            while i < n {
                let c = b[i];
                if in_str {
                    if c == q && b[i - 1] != b'\\' {
                        in_str = false;
                    }
                    i += 1;
                } else if c == b'"' || c == b'\'' {
                    in_str = true;
                    q = c;
                    i += 1;
                } else if c == b' ' || (c == b'/' && i + 1 < n && b[i + 1] == b'/') {
                    break;
                } else {
                    i += 1;
                }
            }
            // The first token starts at column 0 (shared by all siblings at this
            // indent) -- not an alignment point; its end still is.
            if tok_start != 0 {
                bounds.insert(tok_start);
            }
            bounds.insert(i);
            if gap >= 2 && gap_start != 0 {
                padded.insert(tok_start);
                padded.insert(i);
            }
        }
        (bounds, padded)
    }

    /// Indices belonging to a hand-aligned run: a maximal run of >=2 `eligible`
    /// siblings where each adjacent pair shares a token-boundary column padded on
    /// at least one side (the author lined a column up). `skip` items (e.g. a
    /// trailing-comment `Comment` item re-emitted elsewhere) are transparent --
    /// they neither align nor break a run. Callers replace these indices' docs
    /// with a VERBATIM source slice, preserving the column alignment structural
    /// rendering would collapse. Verbatim-slicing a non-aligned line would
    /// suppress normalization (`x=1` -> `x = 1`), so only detected runs qualify.
    fn aligned_indices(&self, spans: &[Span], eligible: &[bool], skip: &[bool]) -> Vec<usize> {
        let n = spans.len();
        let cols = |i: usize| self.aligned_columns(self.slice(spans[i]));
        let mut result = Vec::new();
        let mut i = 0;
        while i < n {
            if !eligible[i] {
                i += 1;
                continue;
            }
            let (mut ps, mut pp) = cols(i);
            let mut members = vec![i];
            let mut j = i + 1;
            loop {
                while j < n && skip[j] {
                    j += 1;
                }
                if j >= n || !eligible[j] {
                    break;
                }
                let (sj, pj) = cols(j);
                if !ps.intersection(&sj).any(|c| pp.contains(c) || pj.contains(c)) {
                    break;
                }
                members.push(j);
                ps = sj;
                pp = pj;
                j += 1;
            }
            if members.len() >= 2 {
                i = members.last().unwrap() + 1;
                result.extend(members);
            } else {
                i += 1;
            }
        }
        result
    }

    /// The source occupying `span`, sliced verbatim (trailing whitespace trimmed)
    /// -- used to preserve a hand-aligned line exactly.
    fn verbatim_line(&self, span: Span) -> Doc {
        Doc::text(self.slice(span).trim_end().to_string())
    }

    /// Like `verbatim_line` but extends to the end of the source line, so a
    /// trailing `// comment` is carried verbatim (no sentinel) -- the trailing-
    /// comment aligner then leaves it alone, preserving the author's own
    /// `//`-column alignment within a hand-aligned run.
    fn verbatim_full_line(&self, span: Span) -> Doc {
        let b = self.src.as_bytes();
        let mut end = span.offset_range.1.min(b.len());
        while end < b.len() && b[end] != b'\n' {
            end += 1;
        }
        Doc::text(self.slice_range(span.offset_range.0, end).trim_end().to_string())
    }

    fn block_has_braces(&self, b: Idx<BlockData>) -> bool {
        self.slice(self.ast.b(&b).span).trim_start().starts_with('{')
    }

    /// A single statement that pins its enclosing `block` (fn/lambda body) to the
    /// multi-line form: control-flow keeps a definition body expanded rather than
    /// collapsing `fn f() { for .. {} }` onto one line.
    fn stmt_forces_multiline(&self, s: Idx<StatementData>) -> bool {
        match &self.ast.s(&s).kind {
            StatementKind::For(_)
            | StatementKind::While(_)
            | StatementKind::WhileLet(_)
            | StatementKind::LifecycleHook { .. }
            | StatementKind::Comment(_) => true,
            StatementKind::Defer(_) => true,
            StatementKind::Expression(e) => matches!(
                &self.ast.e(e).kind,
                ExprKind::If { .. }
                    | ExprKind::Match { .. }
                    | ExprKind::Block(_)
                    | ExprKind::IfLet { .. }
                    | ExprKind::IfLetChain { .. }
                    | ExprKind::SafeNavigationLet { .. }
            ),
            _ => false,
        }
    }

    // ---- statements -------------------------------------------------------

    fn stmt(&self, s: Idx<StatementData>) -> Doc {
        // A single-line statement carrying a block comment -- mid-expression
        // (`let x = 1 + /* c */ 2`) or trailing (`let x = 5 /* note */`, which
        // sits just past the statement span, so extend to cover it) -- has no
        // structural home, so slice it verbatim. Multi-line statements (for/if
        // with bodies) keep structural rendering; their headers are handled by
        // `header_verbatim`.
        let mut span = self.ast.s(&s).span;
        if let Some((cs, ce)) = self.trivia.inline_block_starting_after(span.offset_range.1) {
            let gap = self.slice_range(span.offset_range.1, cs);
            if gap.bytes().all(|b| b == b' ' || b == b'\t') {
                span = Span {
                    offset_range: (span.offset_range.0, ce),
                    ..span
                };
            }
        }
        self.maybe_verbatim_line(span, || self.stmt_structural(s))
    }

    fn stmt_structural(&self, s: Idx<StatementData>) -> Doc {
        match &self.ast.s(&s).kind {
            StatementKind::Expression(e) => self.expr(*e),
            // `use` prints verbatim (no internal layout decisions).
            StatementKind::Use(u) => self.raw(u.span),
            StatementKind::Let(l) => self.let_stmt(l),
            StatementKind::Static(st) => self.static_stmt(st),
            StatementKind::NestedFn(f) => {
                self.header_verbatim(self.ast.s(&s).span, f.body, || self.nested_fn_stmt(f))
            }
            StatementKind::Return { value, .. } => match value {
                Some(v) => Doc::concat([Doc::text("return "), self.expr(*v)]),
                None => Doc::text("return"),
            },
            StatementKind::Break { .. } => Doc::text("break"),
            StatementKind::Continue { .. } => Doc::text("continue"),
            StatementKind::For(f) => {
                self.header_verbatim(self.ast.s(&s).span, f.body, || self.for_stmt(f))
            }
            StatementKind::While(w) => {
                // `loop { .. }` parses as `while true { .. }` (same AST). Render
                // an always-true condition back as `loop`, normalizing the
                // literal `while true` spelling to it too (corpus rule).
                let is_loop = matches!(self.ast.e(&w.condition).kind, ExprKind::Bool(true));
                self.header_verbatim(self.ast.s(&s).span, w.body, || {
                    if is_loop {
                        Doc::concat([Doc::text("loop "), self.flow_block(w.body)])
                    } else {
                        Doc::concat([
                            Doc::text("while "),
                            self.expr(w.condition),
                            Doc::text(" "),
                            self.flow_block(w.body),
                        ])
                    }
                })
            }
            StatementKind::WhileLet(w) => Doc::concat([
                Doc::text("while let "),
                self.pattern(&w.pattern),
                Doc::text(" = "),
                self.expr(w.value),
                Doc::text(" "),
                self.flow_block(w.body),
            ]),
            StatementKind::Comment(text) => self.comment_node(self.ast.s(&s).span, text),
            StatementKind::Defer(_) | StatementKind::LifecycleHook { .. } => {
                self.raw(self.ast.s(&s).span)
            }
        }
    }

    fn let_stmt(&self, l: &Let) -> Doc {
        let mut parts = vec![Doc::text("let "), self.binding(&l.binding)];
        if let Some(ty) = l.ty {
            parts.push(Doc::text(": "));
            parts.push(self.type_expr(ty));
        }
        if let Some(v) = l.value {
            parts.push(Doc::text(" = "));
            parts.push(self.expr(v));
        }
        Doc::concat(parts)
    }

    fn nested_fn_stmt(&self, f: &NestedFn) -> Doc {
        let mut sig = vec![Doc::text("fn "), Doc::text(kw(f.name.name)), self.params(&f.params)];
        if let Some(ret) = f.return_ty {
            sig.push(Doc::text(" -> "));
            sig.push(self.type_expr(ret));
        }
        sig.push(Doc::text(" "));
        Doc::concat([Doc::concat(sig), self.block(f.body)])
    }

    fn static_stmt(&self, st: &Static) -> Doc {
        let mut parts = vec![Doc::text("static "), Doc::text(st.name.name.as_str().to_string())];
        if let Some(ty) = st.ty {
            parts.push(Doc::text(": "));
            parts.push(self.type_expr(ty));
        }
        parts.push(Doc::text(" = "));
        parts.push(self.expr(st.value));
        Doc::concat(parts)
    }

    /// A block comment in a control-flow HEADER (`for /* c */ v in /* c */ xs {`,
    /// `if /* c */ cond {`) has no structural home. Slice the header (statement
    /// start .. body `{`) verbatim, then render the body structurally. Only when
    /// the header is a single source line; else fall back.
    fn header_verbatim(
        &self,
        stmt_span: Span,
        body: Idx<BlockData>,
        fallback: impl FnOnce() -> Doc,
    ) -> Doc {
        let body_span = self.ast.b(&body).span;
        let body_open = body_span.offset_range.0;
        if body_span.line_range.0 == stmt_span.line_range.0
            && self
                .trivia
                .has_block_comment_in(stmt_span.offset_range.0, body_open)
        {
            let header = self.slice_range(stmt_span.offset_range.0, body_open);
            return Doc::concat([
                Doc::text(header.trim_end().to_string()),
                Doc::text(" "),
                self.flow_block(body),
            ]);
        }
        fallback()
    }

    fn for_stmt(&self, f: &For) -> Doc {
        let mut parts = vec![Doc::text("for "), self.binding(&f.var)];
        if let Some(ty) = f.ty {
            parts.push(Doc::text(": "));
            parts.push(self.type_expr(ty));
        }
        parts.push(Doc::text(" in "));
        parts.push(self.expr(f.iter));
        parts.push(Doc::text(" "));
        parts.push(self.flow_block(f.body));
        Doc::concat(parts)
    }

    fn binding(&self, b: &Binding) -> Doc {
        match b {
            Binding::Ident(id) => Doc::text(kw(id.name)),
            Binding::Tuple(items) => Doc::concat([
                Doc::text("("),
                Doc::join(Doc::text(", "), items.iter().map(|b| self.binding(b))),
                Doc::text(")"),
            ]),
            Binding::Newtype { name, binding } => {
                Doc::text(format!("{}({})", kw(name.name), kw(binding.name)))
            }
        }
    }

    // ---- expressions ------------------------------------------------------

    fn expr(&self, e: Idx<ExprData>) -> Doc {
        let data = self.ast.e(&e);
        match &data.kind {
            // Atoms: take the source spelling verbatim (preserves literal form,
            // e.g. float spelling, string escapes, hex case).
            // Integer literals are verbatim except the base prefix, which is
            // lowercased (`0X`/`0B`/`0O` -> `0x`/`0b`/`0o`); digit case is kept.
            ExprKind::Int(..) => Doc::text(normalize_int(self.slice(data.span).trim_end())),

            ExprKind::Null
            | ExprKind::Unit
            | ExprKind::Hole(..)
            | ExprKind::Ident(_)
            | ExprKind::QualifiedIdent { .. }
            | ExprKind::Float(_)
            | ExprKind::String(_)
            | ExprKind::InterpolatedString(_)
            | ExprKind::Bool(_)
            | ExprKind::SiblingFieldRef(_)
            | ExprKind::FunctionRef(_)
            | ExprKind::Path { .. }
            | ExprKind::PathStructLiteral { .. }
            | ExprKind::AnaphoricIdent(_) => self.raw(data.span),

            ExprKind::Paren(inner) => {
                Doc::concat([Doc::text("("), self.expr(*inner), Doc::text(")")])
            }
            ExprKind::Not(x) => Doc::concat([Doc::text("!"), self.expr(*x)]),
            ExprKind::BitNot(x) => Doc::concat([Doc::text("~"), self.expr(*x)]),
            ExprKind::UnaryMinus(x) => Doc::concat([Doc::text("-"), self.expr(*x)]),
            ExprKind::QuestionMark(x) => Doc::concat([self.expr(*x), Doc::text("?")]),
            ExprKind::PreIncrement(x) => Doc::concat([Doc::text("++"), self.expr(*x)]),
            ExprKind::PostIncrement(x) => Doc::concat([self.expr(*x), Doc::text("++")]),
            ExprKind::PreDecrement(x) => Doc::concat([Doc::text("--"), self.expr(*x)]),
            ExprKind::PostDecrement(x) => Doc::concat([self.expr(*x), Doc::text("--")]),
            ExprKind::Spread(x) => Doc::concat([Doc::text("..."), self.expr(*x)]),

            ExprKind::As(x, ty) => {
                Doc::concat([self.expr(*x), Doc::text(" as "), self.type_expr(*ty)])
            }
            ExprKind::Bitcast(x, ty) => Doc::concat([
                Doc::text("bitcast<"),
                self.type_expr(*ty),
                Doc::text(">("),
                self.expr(*x),
                Doc::text(")"),
            ]),
            ExprKind::TypeAscription(x, ty) => {
                Doc::concat([self.expr(*x), Doc::text(" :: "), self.type_expr(*ty)])
            }
            // The modules-v1 spelling round-trips as the parenthesized form:
            // an infix re-print would not re-parse in v1 sources.
            ExprKind::ParenAscription(x, ty) => Doc::concat([
                Doc::text("("),
                self.expr(*x),
                Doc::text(" : "),
                self.type_expr(*ty),
                Doc::text(")"),
            ]),

            ExprKind::FieldAccess(x, field) => {
                Doc::concat([self.expr(*x), Doc::text(format!(".{}", kw(field.name)))])
            }
            ExprKind::SafeNavigation(x, field) => {
                Doc::concat([self.expr(*x), Doc::text(format!("?.{}", kw(field.name)))])
            }
            ExprKind::Index(x, idx) => Doc::concat([
                self.expr(*x),
                Doc::text("["),
                self.expr(*idx),
                Doc::text("]"),
            ]),

            // Preserve the author's layout: a binary expression written across
            // source lines is kept VERBATIM (the author's operator placement --
            // leading or trailing -- and column alignment), NOT collapsed to one
            // line regardless of width and NOT restructured (a leading `-`/`+`
            // would reparse as unary). A single-line one renders structurally.
            ExprKind::BinOp { left, op, right } => {
                if data.span.line_range.0 != data.span.line_range.1 {
                    self.raw(data.span)
                } else {
                    Doc::concat([self.expr(*left), binop_sep(op), self.expr(*right)])
                }
            }

            ExprKind::FunctionCall { func, args, syntax, .. } => match syntax {
                // Overloadable operators (`+ - * / %` ...) are parsed as calls
                // with binary syntax -- render them infix. A multi-line binary is
                // kept verbatim (see ExprKind::BinOp above).
                CallSyntax::Binary(op) if args.len() == 2 => {
                    if data.span.line_range.0 != data.span.line_range.1 {
                        self.raw(data.span)
                    } else {
                        Doc::concat([
                            self.expr(args[0].value),
                            binop_sep(op),
                            self.expr(args[1].value),
                        ])
                    }
                }
                CallSyntax::At => self.at_call(*func, args, data.span),
                // No-parens surface form (`start_game "mag"` == `start_game("mag")`):
                // re-emit it as `callee arg arg` rather than normalizing to parens.
                CallSyntax::NoParens => Doc::concat([
                    self.expr(*func),
                    Doc::text(" "),
                    Doc::join(Doc::text(" "), args.iter().map(|a| self.expr(a.value))),
                ]),
                _ => self.call(*func, args, data.span),
            },

            ExprKind::ArrayLiteral(items) => self.list("[", "]", items, data.span),
            ExprKind::Tuple(items) => self.tuple(items, data.span),

            ExprKind::StructLiteral { name, bases, fields } => {
                // Functional-update literals (`Foo { ..base, x: 1 }`) slice
                // verbatim -- the structural struct_literal path doesn't carry
                // bases yet, and passthrough guarantees round-trip.
                if bases.is_empty() {
                    self.struct_literal(kw(name.name), fields, data.span)
                } else {
                    self.raw(data.span)
                }
            }
            ExprKind::EnumVariantRecord {
                enum_name,
                variant_name,
                fields,
            } => self.struct_literal(
                format!("{}::{}", kw(enum_name.name), kw(variant_name.name)),
                fields,
                data.span,
            ),

            ExprKind::Block(b) => self.block(*b),

            ExprKind::If {
                branches,
                else_branch,
            } => self.if_expr(branches, *else_branch, data.span),

            ExprKind::ReturnExpr(v) => match v {
                Some(v) => Doc::concat([Doc::text("return "), self.expr(*v)]),
                None => Doc::text("return"),
            },
            ExprKind::BreakExpr => Doc::text("break"),
            ExprKind::ContinueExpr => Doc::text("continue"),
            ExprKind::Yield(v) => match v {
                Some(v) => Doc::concat([Doc::text("yield "), self.expr(*v)]),
                None => Doc::text("yield"),
            },

            ExprKind::Assign {
                target,
                value,
                compound_op,
            } => {
                let sym = match compound_op {
                    Some(o) => binop_str(o),
                    None => "=".to_string(),
                };
                Doc::concat([self.expr(*target), Doc::text(format!(" {sym} ")), self.expr(*value)])
            }

            ExprKind::Match { scrutinee, arms } => self.match_expr(*scrutinee, arms, data.span),

            ExprKind::Lambda {
                params,
                return_ty,
                body,
            } => self.lambda(params, *return_ty, *body),

            // Forms not yet built structurally -- slice verbatim so output is
            // never corrupt. Refined in later phases.
            ExprKind::IfLet { .. }
            | ExprKind::IfLetChain { .. }
            | ExprKind::SafeNavigationLet { .. }
            | ExprKind::YieldDelegate(_) => self.raw(data.span),
        }
    }

    fn lambda(
        &self,
        params: &[LambdaParam],
        return_ty: Option<Idx<TypeExprData>>,
        body: Idx<BlockData>,
    ) -> Doc {
        // A single untyped param drops its parens: `(x) => e` -> `x => e`.
        let param_doc = if params.len() == 1 && params[0].ty.is_none() {
            Doc::text(kw(params[0].name.name))
        } else {
            let items = params.iter().map(|p| match p.ty {
                Some(t) => Doc::concat([
                    Doc::text(format!("{}: ", kw(p.name.name))),
                    self.type_expr(t),
                ]),
                None => Doc::text(kw(p.name.name)),
            });
            // Lambda params stay flat (they're short, and breaking empty `()`
            // produced a stray blank line on a wide enclosing line).
            Doc::concat([
                Doc::text("("),
                Doc::join(Doc::text(", "), items),
                Doc::text(")"),
            ])
        };
        let mut head = vec![param_doc];
        if let Some(ret) = return_ty {
            head.push(Doc::text(" -> "));
            head.push(self.type_expr(ret));
        }
        head.push(Doc::text(" => "));

        // Brace form `=> { .. }` vs expression form `=> expr`. The body is always
        // a block in the AST; the source tells us which the author wrote.
        if !self.block_has_braces(body) {
            if let Some(e) = self.arrow_body_expr(body) {
                head.push(self.expr(e));
                return Doc::concat(head);
            }
        }
        if self.ast.b(&body).statements.is_empty() {
            head.push(Doc::text("{}"));
            return Doc::concat(head);
        }
        head.push(self.block(body));
        Doc::concat(head)
    }

    /// For an arrow body the author wrote without braces (`x => expr`), the
    /// parser stores a one-statement block holding either the expression or a
    /// synthesized `return expr`. Recover that expression.
    fn arrow_body_expr(&self, body: Idx<BlockData>) -> Option<Idx<ExprData>> {
        let stmts = &self.ast.b(&body).statements;
        if stmts.len() != 1 {
            return None;
        }
        match &self.ast.s(&stmts[0]).kind {
            StatementKind::Expression(e) => Some(*e),
            StatementKind::Return { value: Some(e), .. } => Some(*e),
            _ => None,
        }
    }

    fn match_expr(&self, scrutinee: Idx<ExprData>, arms: &[MatchArm], span: Span) -> Doc {
        // Preserve the author's layout: a match written on one source line stays
        // inline (`match p { A => e, _ => null }`); multi-line expands one arm
        // per line.
        if span.line_range.0 == span.line_range.1 {
            return Doc::concat([
                Doc::text("match "),
                self.expr(scrutinee),
                Doc::text(" { "),
                Doc::join(Doc::text(", "), arms.iter().map(|a| self.match_arm(a))),
                Doc::text(" }"),
            ]);
        }
        let bodies: Vec<Span> = arms.iter().map(|a| self.ast.b(&a.body).span).collect();
        // Full arm span (pattern .. body): the parser drops pattern spans, so the
        // arm's start is recovered as the first token on the body's source line.
        // Used only for alignment detection/slicing of single-line arms.
        let arm_spans: Vec<Span> = bodies
            .iter()
            .map(|b| {
                let start = self.line_first_token_offset(b.offset_range.0);
                Span {
                    offset_range: (start, b.offset_range.1),
                    ..*b
                }
            })
            .collect();
        let eligible: Vec<bool> = bodies.iter().map(|b| b.line_range.0 == b.line_range.1).collect();
        let mut docs: Vec<Doc> = arms.iter().map(|a| self.match_arm(a)).collect();
        // Preserve a hand-aligned run of arms (`Variant   => 0` columns); the
        // trailing comment is added by comma_block, not folded into `docs`.
        let no_skip = vec![false; arm_spans.len()];
        for k in self.aligned_indices(&arm_spans, &eligible, &no_skip) {
            docs[k] = self.verbatim_line(arm_spans[k]);
        }
        // A block comment anywhere in a single-line arm (`1 /* c */ => e`,
        // `_ => /* c */ e`) has no structural home -- slice the arm verbatim.
        for k in 0..arm_spans.len() {
            if eligible[k]
                && self
                    .trivia
                    .has_block_comment_in(arm_spans[k].offset_range.0, arm_spans[k].offset_range.1)
            {
                docs[k] = self.verbatim_line(arm_spans[k]);
            }
        }
        // Entry span is the FULL arm (pattern .. body), so comma_block's
        // leading-block prepend checks the arm start, not the body -- an interior
        // block comment already lives in the verbatim doc and must not double.
        let entries = docs
            .into_iter()
            .zip(&arm_spans)
            .map(|(d, s)| (d, s.line_range.0, *s))
            .collect();
        Doc::concat([
            Doc::text("match "),
            self.expr(scrutinee),
            Doc::text(" "),
            self.comma_block(entries, Some((span.line_range.0, span.line_range.1)), "{", "}"),
        ])
    }

    fn match_arm(&self, arm: &MatchArm) -> Doc {
        let mut parts = vec![self.pattern(&arm.pattern)];
        if let Some(g) = arm.guard {
            parts.push(Doc::text(" if "));
            parts.push(self.expr(g));
        }
        parts.push(Doc::text(" => "));
        // Arm body: `=> expr` if the author wrote no braces, else a block that
        // PRESERVES the author's layout -- a single-line `=> { stmt }` stays
        // inline, a multi-line block stays expanded.
        if !self.block_has_braces(arm.body) {
            let stmts = &self.ast.b(&arm.body).statements;
            if stmts.len() == 1 {
                match &self.ast.s(&stmts[0]).kind {
                    StatementKind::Expression(e) => {
                        parts.push(self.expr(*e));
                        return Doc::concat(parts);
                    }
                    // `=> e` desugars to a synthetic `return e`, so the parser
                    // can't distinguish it from an EXPLICIT `=> return e` -- which
                    // returns from the enclosing fn, NOT the arm value. Keep the
                    // keyword when it actually precedes the value in source.
                    StatementKind::Return { value: Some(e), .. } => {
                        let stmt_start = self.ast.s(&stmts[0]).span.offset_range.0;
                        let val_start = self.ast.e(e).span.offset_range.0;
                        if stmt_start < val_start {
                            parts.push(Doc::text("return "));
                        }
                        parts.push(self.expr(*e));
                        return Doc::concat(parts);
                    }
                    _ => {}
                }
            }
        }
        parts.push(self.flow_block(arm.body));
        Doc::concat(parts)
    }

    /// `f(args) @ rhs` -- the parser desugars this to `f(args, () => rhs)` with
    /// `At` syntax, so the last arg is the synthesized `@`-lambda. Reconstruct
    /// the surface form (normalizing `@` spacing).
    fn at_call(&self, func: Idx<ExprData>, args: &[FunctionCallArg], span: Span) -> Doc {
        let Some((lam, others)) = args.split_last() else {
            return self.raw(span);
        };
        let ExprKind::Lambda { body, .. } = &self.ast.e(&lam.value).kind else {
            return self.raw(span);
        };
        let body = *body;
        let f = self.ast.e(&func).span;
        let (head, head_span) = if others.is_empty() {
            (self.expr(func), f)
        } else {
            // The head call's own span (callee .. last non-lambda arg), NOT the
            // whole `f(args) @ {...}` span -- otherwise the multi-line `@`-block
            // makes the preserve check expand the head call's args.
            let last = self.ast.e(&others.last().unwrap().value).span;
            let head_span = Span {
                file: f.file,
                line_range: (f.line_range.0, last.line_range.1),
                col_range: (f.col_range.0, last.col_range.1),
                offset_range: (f.offset_range.0, last.offset_range.1),
            };
            (self.call(func, others, head_span), head_span)
        };
        let stmts = &self.ast.b(&body).statements;
        let rhs_expr = if !self.block_has_braces(body) {
            self.arrow_body_expr(body)
        } else if stmts.len() == 1 {
            // `f() @ { stmt }` unwraps a single expression statement to the bare
            // form `f() @ stmt` (the braces add nothing).
            match &self.ast.s(&stmts[0]).kind {
                StatementKind::Expression(e) => Some(*e),
                _ => None,
            }
        } else {
            None
        };
        let rhs = rhs_expr
            .map(|e| self.expr(e))
            .unwrap_or_else(|| self.block(body));
        // A `//` comment on the `@` line (`head @ // note\n rhs`) has no
        // structural home once the parser desugars `@` away -- recover it as the
        // `@`-line trailing comment and keep the rhs on the next line. Gated on
        // the rhs actually starting on a LATER line: a genuine `@`-line comment
        // eats the rest of that line, forcing rhs down. Without that guard a
        // single-line `head @ rhs // note` mis-claims the statement's own
        // trailing comment here AND lets the statement handler emit it -> dup.
        let rhs_line = rhs_expr
            .map(|e| self.ast.e(&e).span.line_range.0)
            .unwrap_or_else(|| self.ast.b(&body).span.line_range.0);
        let at_comment = self
            .trivia
            .trailing_comment(head_span.line_range.1)
            .filter(|(_, _, off)| *off >= head_span.offset_range.1);
        if let Some((text, _, _)) = at_comment.filter(|_| rhs_line > head_span.line_range.1) {
            return Doc::concat([
                head,
                Doc::text(" @ "),
                self.comment_line(text),
                hardline(),
                rhs,
            ]);
        }
        Doc::concat([head, Doc::text(" @ "), rhs])
    }

    fn call(&self, func: Idx<ExprData>, args: &[FunctionCallArg], span: Span) -> Doc {
        // `a.b().c().d()` -- a chain of >=2 method calls -- lays out one call per
        // line when it doesn't fit, instead of collapsing the chain and breaking
        // the tail call's args.
        if let Some(chain) = self.method_chain(func, args, span) {
            return chain;
        }
        Doc::concat([self.expr(func), self.call_args(args, span)])
    }

    /// The `(args)` part of a call -- everything after the callee. Split out so a
    /// method-chain segment (`.method(args)`) can reuse it.
    fn call_args(&self, args: &[FunctionCallArg], span: Span) -> Doc {
        if args.is_empty() {
            return Doc::text("()");
        }
        let arg_doc = |a: &FunctionCallArg| {
            let v = self.expr(a.value);
            if a.is_spread {
                return Doc::concat([Doc::text(".."), v]);
            }
            let v = if a.is_splat {
                Doc::concat([v, Doc::text("...")])
            } else {
                v
            };
            match &a.name {
                Some(n) => Doc::concat([Doc::text(format!("{}: ", kw(n.name))), v]),
                None => v,
            }
        };
        // An arg trailing comment (`source.w, source.h,  // base size`), or a
        // standalone comment among the args, forces the comment-preserving multi-
        // line layout.
        let arg_span = |a: &FunctionCallArg| self.ast.e(&a.value).span;
        let has_comments = args
            .iter()
            .any(|a| self.claimed_trailing_comment(arg_span(a)).is_some())
            || !self
                .trivia
                .standalone_comments_between(span.line_range.0, span.line_range.1)
                .is_empty()
            || self.trivia.has_block_comment_in(span.offset_range.0, span.offset_range.1);
        // Preserve the author's layout: a call written across multiple source
        // lines expands one arg per line; one on a single line stays inline --
        // even past the width limit, so a hand-compacted call (e.g. inside a
        // data-table row) is kept, not exploded. Comments force the multi-line
        // form (so they survive).
        let source_multiline = span.line_range.0 != span.line_range.1;
        // Trailing-argument hug: when the author wrote the call with a sole or
        // last collection/struct-literal argument opening on the call line
        // (`push(Foo {`, `arena_insert(prefabs, Prefab {`), keep it hugging --
        // the arg's own delimiters break against the parens instead of indenting
        // the whole argument an extra level. Source-aware so a deliberately
        // broken call (`f(\n  Foo { .. }\n)`) is preserved too.
        // Not gated on `!has_comments`: the hugging arg renders its OWN interior
        // comments (`insert(Corpse { ..  // note\n .. })`), and if a comment
        // preceded the arg it wouldn't be on the call line, so it wouldn't hug.
        if source_multiline && self.hugs_trailing_arg(args, span) {
            let n = args.len();
            let mut parts = vec![Doc::text("(")];
            for a in &args[..n - 1] {
                parts.push(arg_doc(a));
                parts.push(Doc::text(", "));
            }
            parts.push(arg_doc(&args[n - 1]));
            parts.push(Doc::text(")"));
            return Doc::concat(parts);
        }
        // Preserve the author's per-line argument grouping (hanging or exploded).
        if !has_comments && source_multiline {
            if let Some(d) = self.call_arg_rows(args, span) {
                return d;
            }
        }
        if has_comments || source_multiline {
            let entries = args
                .iter()
                .map(|a| {
                    let s = arg_span(a);
                    (arg_doc(a), s.line_range.0, s)
                })
                .collect();
            let body = if has_comments {
                Some((span.line_range.0, span.line_range.1))
            } else {
                None
            };
            return self.comma_block(entries, body, "(", ")");
        }
        // Single-line source: stay inline.
        Doc::concat([
            Doc::text("("),
            Doc::join(Doc::text(", "), args.iter().map(arg_doc)),
            Doc::text(")"),
        ])
    }

    /// Render `recv.m1(..).m2(..)...` as a member chain when it has >=2 method
    /// calls: flat if it fits, else the head on the first line and each remaining
    /// `.method(args)` on its own indented line. Returns `None` when `func` isn't
    /// a method access (so it isn't a chain).
    fn method_chain(
        &self,
        func: Idx<ExprData>,
        args: &[FunctionCallArg],
        span: Span,
    ) -> Option<Doc> {
        // Collect segments outermost-first by walking the receiver spine.
        let mut segs: Vec<(Ustr, &[FunctionCallArg], Span)> = Vec::new();
        let mut cur_func = func;
        let mut cur_args = args;
        let mut cur_span = span;
        let base = loop {
            let ExprKind::FieldAccess(recv, method) = &self.ast.e(&cur_func).kind else {
                return None;
            };
            // The segment's OWN call span (method name -> closing `)`), not the
            // cumulative receiver chain. call_args reads `line_range` to decide
            // whether to expand the arg list; using the cumulative span made that
            // depend on how the chain itself was broken across lines -- so once
            // the formatter put a segment on its own line, the next pass saw a
            // two-line cumulative span and expanded the (single-line) args, an
            // oscillation (mag `level_editor.rock`). The own span tracks only the
            // author's arg-list layout, which is stable under chain reflow.
            let own_span = Span {
                line_range: (method.span.line_range.0, cur_span.line_range.1),
                col_range: (method.span.col_range.0, cur_span.col_range.1),
                offset_range: (method.span.offset_range.0, cur_span.offset_range.1),
                ..cur_span
            };
            segs.push((method.name, cur_args, own_span));
            // Continue the spine only while the receiver is itself a method call.
            if let ExprKind::FunctionCall {
                func: rf,
                args: ra,
                syntax: CallSyntax::Normal,
                ..
            } = &self.ast.e(recv).kind
            {
                if matches!(&self.ast.e(rf).kind, ExprKind::FieldAccess(..)) {
                    cur_func = *rf;
                    cur_args = ra;
                    cur_span = self.ast.e(recv).span;
                    continue;
                }
            }
            break *recv;
        };
        // A single `.method(..)` the author broke onto its own line still lays
        // out as a chain (`recv(..)\n    .method(..)`), preserving that break --
        // the regular call path would join them and explode the method's args.
        let broken_single = segs.len() == 1
            && segs[0].2.line_range.0 > self.ast.e(&base).span.line_range.1;
        if segs.len() < 2 && !broken_single {
            return None;
        }
        segs.reverse();
        let seg_doc =
            |(m, a, s): &(Ustr, &[FunctionCallArg], Span)| {
                Doc::concat([Doc::text(format!(".{}", kw(*m))), self.call_args(a, *s)])
            };
        // A call receiver (`factory().a().b()`) -- or a single broken `.method` --
        // keeps the base alone on the first line; a plain receiver (`r.a().b()`)
        // keeps its first call with it.
        let base_is_call = matches!(&self.ast.e(&base).kind, ExprKind::FunctionCall { .. });
        let (head, tail) = if base_is_call || broken_single {
            (self.expr(base), &segs[..])
        } else {
            (Doc::concat([self.expr(base), seg_doc(&segs[0])]), &segs[1..])
        };
        // Preserve the author's layout: a chain written across multiple source
        // lines breaks one call per line; one on a single line stays inline.
        let source_multiline = span.line_range.0 != span.line_range.1;
        let tail_docs: Vec<Doc> = if source_multiline {
            tail.iter()
                .map(|seg| Doc::concat([hardline(), seg_doc(seg)]))
                .collect()
        } else {
            tail.iter().map(seg_doc).collect()
        };
        Some(Doc::concat([head, Doc::indent(Doc::concat(tail_docs))]))
    }

    /// An argument that "hugs" the call parens when it's the sole argument: a
    /// collection, struct literal or block lambda whose own delimiters provide line breaks,
    /// so `f(Foo {\n  ..\n})` / `f([\n  ..\n])` keep the opening brace on the call
    /// line rather than indenting the whole argument an extra level.
    fn is_huggable_arg(&self, e: Idx<ExprData>) -> bool {
        if let ExprKind::Lambda { body, .. } = &self.ast.e(&e).kind {
            return self.block_has_braces(*body);
        }
        matches!(
            &self.ast.e(&e).kind,
            ExprKind::ArrayLiteral(_)
                | ExprKind::StructLiteral { .. }
                | ExprKind::EnumVariantRecord { .. }
        )
    }

    /// The author wrote the trailing argument hugging the call parens: the last
    /// arg is a huggable literal opening on the same source line as the
    /// preceding content (the `(` for a sole arg, else the previous arg). Prior
    /// args must be single-line so they sit inline before the hug. Source-aware,
    /// so it preserves both a hugged call and a deliberately-broken one.
    fn hugs_trailing_arg(&self, args: &[FunctionCallArg], span: Span) -> bool {
        let Some(last) = args.last() else { return false };
        if last.name.is_some() || last.is_splat || last.is_spread || !self.is_huggable_arg(last.value)
        {
            return false;
        }
        let prior = &args[..args.len() - 1];
        if prior.iter().any(|a| {
            let s = self.ast.e(&a.value).span;
            s.line_range.0 != s.line_range.1
        }) {
            return false;
        }
        let last_line = self.ast.e(&last.value).span.line_range.0;
        let prev_line = match prior.last() {
            Some(p) => self.ast.e(&p.value).span.line_range.1,
            None => span.line_range.0,
        };
        last_line == prev_line
    }

    /// A multi-line call's arguments, preserving the author's per-line grouping.
    /// Args that shared a source line are sliced verbatim together; the first
    /// group hugs `(` when the author opened the call on that line (`f(a, b,` ...),
    /// else it starts on its own indented line (`f(\n  a, ...`), and the close `)`
    /// hugs the last arg when the author wrote it there. Produces the same
    /// exploded shape as `comma_block` for a one-arg-per-line call (so those don't
    /// churn). `None` -- caller falls back to `comma_block` -- when any argument
    /// spans multiple source lines or is a splat (the verbatim slice can't carry
    /// those).
    fn call_arg_rows(&self, args: &[FunctionCallArg], span: Span) -> Option<Doc> {
        // (start_line, end_line, start_offset, end_offset) per arg, or None if the
        // arg isn't a single-line non-splat value.
        let info = |a: &FunctionCallArg| -> Option<(usize, usize, usize, usize)> {
            if a.is_splat || a.is_spread {
                return None;
            }
            let v = self.ast.e(&a.value).span;
            let (start_line, start_off) = match &a.name {
                Some(n) => (n.span.line_range.0, n.span.offset_range.0),
                None => (v.line_range.0, v.offset_range.0),
            };
            if start_line != v.line_range.1 {
                return None;
            }
            Some((start_line, v.line_range.1, start_off, v.offset_range.1))
        };
        let infos = args.iter().map(info).collect::<Option<Vec<_>>>()?;
        // Group consecutive args sharing a start line.
        let mut groups: Vec<Vec<(usize, usize, usize, usize)>> = Vec::new();
        for i in infos {
            match groups.last_mut() {
                Some(g) if g[0].0 == i.0 => g.push(i),
                _ => groups.push(vec![i]),
            }
        }
        let hanging_open = groups[0][0].0 == span.line_range.0;
        let hanging_close = groups.last().unwrap().last().unwrap().1 == span.line_range.1;
        let trailing = self.source_trailing_comma(self.ast.e(&args.last().unwrap().value).span);
        let g_count = groups.len();
        let mut head: Vec<Doc> = vec![Doc::text("(")];
        let mut body: Vec<Doc> = Vec::new();
        for (gi, group) in groups.iter().enumerate() {
            let lo = group.first().unwrap().2;
            let hi = group.last().unwrap().3;
            let slice = Doc::text(self.slice_range(lo, hi).trim_end().to_string());
            let comma = if gi + 1 < g_count || trailing {
                Doc::text(",")
            } else {
                nil()
            };
            if gi == 0 && hanging_open {
                head.push(slice);
                head.push(comma);
            } else {
                body.push(hardline());
                body.push(slice);
                body.push(comma);
            }
        }
        let close_prefix = if hanging_close { nil() } else { hardline() };
        Some(Doc::concat([
            Doc::concat(head),
            Doc::indent(Doc::concat(body)),
            close_prefix,
            Doc::text(")"),
        ]))
    }

    fn tuple(&self, items: &[Idx<ExprData>], span: Span) -> Doc {
        // Preserve the author's layout (tuples never take a trailing comma): a
        // single-line tuple stays inline; a multi-line one expands one per line.
        if span.line_range.0 == span.line_range.1 {
            return Doc::concat([
                Doc::text("("),
                Doc::join(Doc::text(", "), items.iter().map(|i| self.expr(*i))),
                Doc::text(")"),
            ]);
        }
        Doc::concat([
            Doc::text("("),
            Doc::indent(Doc::concat([
                hardline(),
                Doc::join(
                    Doc::concat([Doc::text(","), hardline()]),
                    items.iter().map(|i| self.expr(*i)),
                ),
            ])),
            hardline(),
            Doc::text(")"),
        ])
    }

    fn list(&self, open: &str, close: &str, items: &[Idx<ExprData>], span: Span) -> Doc {
        if items.is_empty() {
            return Doc::text(format!("{open}{close}"));
        }
        // Preserve the author's layout: an array written across multiple source
        // lines expands one element per line; one on a single line stays inline
        // -- even past the width limit. Same rule as struct literals.
        let force = span.line_range.0 != span.line_range.1;
        if !force {
            return Doc::concat([
                Doc::text(open.to_string()),
                Doc::join(Doc::text(", "), items.iter().map(|i| self.expr(*i))),
                Doc::text(close.to_string()),
            ]);
        }
        // Comment-bearing arrays route through comma_block (it preserves the
        // comments). Otherwise preserve the author's row layout verbatim.
        let has_comments = items
            .iter()
            .any(|e| self.claimed_trailing_comment(self.ast.e(e).span).is_some())
            || !self
                .trivia
                .standalone_comments_between(span.line_range.0, span.line_range.1)
                .is_empty()
            || self.trivia.has_block_comment_in(span.offset_range.0, span.offset_range.1);
        if !has_comments {
            let rows = items
                .iter()
                .map(|e| (self.ast.e(e).span, self.expr(*e)))
                .collect();
            return self.verbatim_rows(rows, open, close);
        }
        // Comment-bearing: comma_block recovers the interior comments, but each
        // single-line element is sliced VERBATIM (not re-rendered structurally),
        // so a hand-aligned data table that happens to carry a `//` comment keeps
        // its column alignment (mobs.rock MOB_KINDS).
        let entries = items
            .iter()
            .map(|e| {
                let s = self.ast.e(e).span;
                let doc = if s.line_range.0 == s.line_range.1 {
                    Doc::text(self.slice(s).trim_end().to_string())
                } else {
                    self.expr(*e)
                };
                (doc, s.line_range.0, s)
            })
            .collect();
        self.comma_block(entries, Some((span.line_range.0, span.line_range.1)), open, close)
    }

    fn struct_literal(
        &self,
        name: String,
        fields: &[(Vec<Ident>, Idx<ExprData>)],
        span: Span,
    ) -> Doc {
        if fields.is_empty() {
            return Doc::text(format!("{name} {{}}"));
        }
        let field_doc = |names: &[Ident], value: &Idx<ExprData>| -> Doc {
            // Shorthand `{ x }` parses as ([x], Ident(x)) where the value shares
            // the key's source span; `{ x: x }` parses the same but with the
            // value at a distinct span. Only collapse the former (preserve an
            // explicit `x: x` the author wrote).
            if names.len() == 1 {
                if let ExprKind::Ident(id) = &self.ast.e(value).kind {
                    if id.name == names[0].name
                        && self.ast.e(value).span.offset_range == names[0].span.offset_range
                    {
                        return Doc::text(kw(id.name));
                    }
                }
            }
            let key =
                Doc::join(Doc::text(" + "), names.iter().map(|n| Doc::text(kw(n.name))));
            Doc::concat([key, Doc::text(": "), self.expr(*value)])
        };
        let field_line = |names: &[Ident]| names.first().map(|n| n.span.line_range.0).unwrap_or(0);
        // A field with a trailing comment, or a standalone comment in the body,
        // forces the comment-preserving multi-line layout (a `// ...` can't sit
        // inside an inline `{ a, b }`). A field claims a trailing comment only if
        // nothing but whitespace/`,` sits between the field's end and the `//`: on
        // a single-line literal the comment trails the `}` and belongs to the
        // enclosing statement (`global g = Foo { a: 1 }  // c`).
        let field_has_comment = |value: &Idx<ExprData>| {
            let v = self.ast.e(value);
            match self.trivia.trailing_comment(v.span.line_range.1) {
                Some((_, _, coff)) => self
                    .slice_range(v.span.offset_range.1, coff)
                    .chars()
                    .all(|c| c.is_whitespace() || c == ',' || c == ';'),
                None => false,
            }
        };
        let has_comments = fields.iter().any(|(_, v)| field_has_comment(v))
            || !self
                .trivia
                .standalone_comments_between(span.line_range.0, span.line_range.1)
                .is_empty()
            || self.trivia.has_block_comment_in(span.offset_range.0, span.offset_range.1);
        // Preserve the author's layout: a struct literal written across multiple
        // source lines expands one-field-per-line; one written on a single line
        // stays inline -- even past the width limit (a hand-compacted data-table
        // row is kept, not exploded). Comments force the multi-line form.
        let source_multiline = span.line_range.0 != span.line_range.1;
        if source_multiline || has_comments {
            // Comment-bearing literals route through comma_block (it preserves
            // the comments). Otherwise preserve the author's row layout verbatim
            // -- fields packed onto one source line stay packed, with their
            // column alignment intact.
            if !has_comments {
                let rows = fields
                    .iter()
                    .map(|(names, value)| {
                        let v = self.ast.e(value).span;
                        // The field's full source span: first key name -> value end.
                        let full = Span {
                            line_range: (names[0].span.line_range.0, v.line_range.1),
                            col_range: (names[0].span.col_range.0, v.col_range.1),
                            offset_range: (names[0].span.offset_range.0, v.offset_range.1),
                            ..v
                        };
                        (full, field_doc(names, value))
                    })
                    .collect();
                return Doc::concat([
                    Doc::text(format!("{name} ")),
                    self.verbatim_rows(rows, "{", "}"),
                ]);
            }
            let entries = fields
                .iter()
                .map(|(names, value)| {
                    let v = self.ast.e(value).span;
                    // Full field span (name start -> value end) so a leading
                    // inline block comment (`/* y field */ y: 2`) is in the
                    // entry's leading gap; the end is unchanged, so trailing
                    // comma/comment detection is unaffected.
                    let full = Span {
                        offset_range: (names[0].span.offset_range.0, v.offset_range.1),
                        ..v
                    };
                    (field_doc(names, value), field_line(names), full)
                })
                .collect();
            return Doc::concat([
                Doc::text(format!("{name} ")),
                self.comma_block(entries, Some((span.line_range.0, span.line_range.1)), "{", "}"),
            ]);
        }
        let entries = fields.iter().map(|(names, value)| field_doc(names, value));
        Doc::concat([
            Doc::text(format!("{name} {{ ")),
            Doc::join(Doc::text(", "), entries),
            Doc::text(" }"),
        ])
    }

    fn if_expr(
        &self,
        branches: &[(Idx<ExprData>, Idx<BlockData>)],
        else_branch: Option<Idx<BlockData>>,
        if_span: Span,
    ) -> Doc {
        // The whole if-expression is ONE group: if it doesn't fit on a line,
        // every branch breaks together (matching the corpus). Each nested `if`
        // is its own group, so it collapses independently when it fits.
        // Preserve the author's break before `else`/`else if`: if the keyword
        // sits on a later source line than the preceding branch body ended, keep
        // it on its own line (`if c { x }\nelse { y }`), else inline (`} else {`).
        let mut parts = Vec::new();
        for (i, (cond, body)) in branches.iter().enumerate() {
            if i == 0 {
                // A block comment in the first header (`if /* c */ cond /* c */ {`)
                // has no structural home -- slice the header verbatim, then render
                // the body structurally.
                let body_span = self.ast.b(body).span;
                if if_span.line_range.0 == body_span.line_range.0
                    && self
                        .trivia
                        .has_block_comment_in(if_span.offset_range.0, body_span.offset_range.0)
                {
                    let header =
                        self.slice_range(if_span.offset_range.0, body_span.offset_range.0);
                    parts.push(Doc::text(header.trim_end().to_string()));
                    parts.push(Doc::text(" "));
                    parts.push(self.branch_body(*body));
                    continue;
                }
                parts.push(Doc::text("if "));
            } else {
                let prev_end = self.ast.b(&branches[i - 1].1).span.line_range.1;
                let here = self.ast.e(cond).span.line_range.0;
                parts.push(if here > prev_end {
                    Doc::concat([hardline(), Doc::text("else if ")])
                } else {
                    Doc::text(" else if ")
                });
            }
            parts.push(self.expr(*cond));
            parts.push(Doc::text(" "));
            parts.push(self.branch_body(*body));
        }
        if let Some(eb) = else_branch {
            let eb_span = self.ast.b(&eb).span;
            let prev_end = self.ast.b(&branches.last().unwrap().1).span.line_range.1;
            if eb_span.line_range.0 > prev_end {
                // `else` on its own line. If its body is single-line, slice the
                // whole `else ... }` line verbatim so the author's brace
                // alignment (`else       { .. }`) survives; otherwise structural.
                parts.push(hardline());
                if eb_span.line_range.0 == eb_span.line_range.1 {
                    let start = self.line_first_token_offset(eb_span.offset_range.0);
                    parts.push(self.verbatim_line(Span {
                        offset_range: (start, eb_span.offset_range.1),
                        ..eb_span
                    }));
                } else {
                    parts.push(Doc::text("else "));
                    parts.push(self.branch_body(eb));
                }
            } else {
                parts.push(Doc::text(" else "));
                parts.push(self.branch_body(eb));
            }
        }
        // No enclosing group: each branch preserves its own source layout (the
        // branch bodies emit hardlines when expanded), so the if isn't reflowed
        // by width -- a one-line `if c { x }` stays one line, a multi-line one
        // stays multi-line.
        Doc::concat(parts)
    }

    /// An if/else branch body. Preserves the author's layout: a single-statement
    /// branch the author wrote on one source line stays inline (`{ stmt }`);
    /// anything multi-line in source (or multi-statement, or carrying its own
    /// trailing comment) expands.
    fn branch_body(&self, block_idx: Idx<BlockData>) -> Doc {
        let block = self.ast.b(&block_idx);
        let stmts = &block.statements;
        let source_single_line = block.span.line_range.0 == block.span.line_range.1;
        if stmts.is_empty() {
            return if source_single_line {
                Doc::text("{}")
            } else {
                Doc::concat([Doc::text("{"), hardline(), Doc::text("}")])
            };
        }
        if let Some(d) = self.inline_single_line_block(block_idx) {
            return d;
        }
        Doc::concat([
            Doc::text("{"),
            Doc::indent(Doc::concat([hardline(), self.stmt_seq(stmts)])),
            hardline(),
            Doc::text("}"),
        ])
    }

    // ---- types ------------------------------------------------------------

    fn type_expr(&self, t: Idx<TypeExprData>) -> Doc {
        let data = self.ast.te(&t);
        // A block comment inside a type (`(int, /* c */ int)`, `Foo<int /* c */>`)
        // has no structural home -- slice the type verbatim.
        if data.span.line_range.0 == data.span.line_range.1
            && self
                .trivia
                .has_block_comment_in(data.span.offset_range.0, data.span.offset_range.1)
        {
            return Doc::text(self.slice(data.span).trim_end().to_string());
        }
        let inner = match &data.kind {
            TypeExprKind::Any => Doc::text("any"),
            TypeExprKind::Unit => Doc::text("()"),
            TypeExprKind::String => Doc::text("string"),
            TypeExprKind::Int => Doc::text("int"),
            TypeExprKind::Uint => Doc::text("uint"),
            TypeExprKind::Float => Doc::text("float"),
            TypeExprKind::F64 => Doc::text("f64"),
            TypeExprKind::F32 => Doc::text("f32"),
            TypeExprKind::Bool => Doc::text("bool"),
            TypeExprKind::Ref(target) => {
                let reference = Doc::concat([Doc::text("ref "), self.type_expr(*target)]);
                if data.is_nullable {
                    Doc::concat([Doc::text("("), reference, Doc::text(")")])
                } else {
                    reference
                }
            }
            TypeExprKind::Ident(id) => Doc::text(kw(id.name)),
            // Paths print verbatim: no internal spacing decisions.
            TypeExprKind::Path(_) | TypeExprKind::AppPath { .. } => self.raw(data.span),
            TypeExprKind::Array(el) => {
                Doc::concat([Doc::text("["), self.type_expr(*el), Doc::text("]")])
            }
            TypeExprKind::App { ctor, args } => Doc::concat([
                Doc::text(kw(ctor.name)),
                Doc::text("<"),
                Doc::join(Doc::text(", "), args.iter().map(|a| self.type_expr(*a))),
                Doc::text(">"),
            ]),
            TypeExprKind::Tuple(tys) => Doc::concat([
                Doc::text("("),
                Doc::join(Doc::text(", "), tys.iter().map(|t| self.type_expr(*t))),
                Doc::text(")"),
            ]),
            // Two source spellings the AST doesn't distinguish: `fn(A) -> B` and
            // `(A) -> B`. Detect the prefix from the span START (reliable), but
            // render params and return STRUCTURALLY -- the function type's own
            // span over-extends past `>>` in `App<.., fn-returning-generic>`
            // (`Map<.., () -> Foo<Bar>>`), so a verbatim slice grabs an extra `>`.
            TypeExprKind::Function { params, ret } => {
                let source = self.slice(data.span).trim_start();
                let starts_fn = source.starts_with("fn")
                    && source[2..].trim_start().starts_with('(');
                let starts_parenthesized_fn = source.starts_with('(')
                    && source[1..].trim_start().starts_with("fn")
                    && {
                        let inner = source[1..].trim_start();
                        inner[2..].trim_start().starts_with('(')
                    };
                let prefix = if starts_parenthesized_fn && !data.is_nullable {
                    "(fn("
                } else if starts_fn || starts_parenthesized_fn {
                    "fn("
                } else {
                    "("
                };
                let suffix = if starts_parenthesized_fn && !data.is_nullable { ")" } else { "" };
                Doc::concat([
                    Doc::text(prefix),
                    Doc::join(Doc::text(", "), params.iter().map(|p| self.type_expr(*p))),
                    Doc::text(") -> "),
                    self.type_expr(*ret),
                    Doc::text(suffix),
                ])
            }
            TypeExprKind::Hole(_, Some(n)) => Doc::text(format!("?{n}")),
            TypeExprKind::Hole(_, None) => Doc::text("?"),
        };
        if data.is_nullable {
            if matches!(data.kind, TypeExprKind::Function { .. }) {
                Doc::concat([Doc::text("("), inner, Doc::text(")?")])
            } else {
                Doc::concat([inner, Doc::text("?")])
            }
        } else {
            inner
        }
    }

    // ---- patterns ---------------------------------------------------------

    fn pattern(&self, p: &Pattern) -> Doc {
        match p {
            Pattern::Wildcard => Doc::text("_"),
            Pattern::Ident(id) => Doc::text(kw(id.name)),
            Pattern::Literal(e) => self.expr(*e),
            Pattern::Tuple(ps) => Doc::concat([
                Doc::text("("),
                Doc::join(Doc::text(", "), ps.iter().map(|p| self.pattern(p))),
                Doc::text(")"),
            ]),
            Pattern::EnumVariant {
                enum_name,
                variant_name,
                args,
            } => {
                let head = match enum_name {
                    Some(e) => format!("{}::{}", kw(e.name), kw(variant_name.name)),
                    None => kw(variant_name.name),
                };
                match args {
                    Some(args) => Doc::concat([
                        Doc::text(head),
                        Doc::text("("),
                        Doc::join(Doc::text(", "), args.iter().map(|p| self.pattern(p))),
                        Doc::text(")"),
                    ]),
                    None => Doc::text(head),
                }
            }
            Pattern::PathVariant { path, args } => {
                let root = if path.from_root { "::" } else { "" };
                let head = format!(
                    "{root}{}",
                    path.segments
                        .iter()
                        .map(|s| kw(s.name))
                        .collect::<Vec<_>>()
                        .join("::")
                );
                match args {
                    Some(args) => Doc::concat([
                        Doc::text(head),
                        Doc::text("("),
                        Doc::join(Doc::text(", "), args.iter().map(|p| self.pattern(p))),
                        Doc::text(")"),
                    ]),
                    None => Doc::text(head),
                }
            }
            Pattern::PathRecord { path, fields, rest } => {
                let root = if path.from_root { "::" } else { "" };
                let head = format!(
                    "{root}{} ",
                    path.segments
                        .iter()
                        .map(|s| kw(s.name))
                        .collect::<Vec<_>>()
                        .join("::")
                );
                let mut entries: Vec<Doc> = fields
                    .iter()
                    .map(|(n, p)| {
                        if let Pattern::Ident(id) = p {
                            if id.name == n.name {
                                return Doc::text(kw(n.name));
                            }
                        }
                        Doc::concat([Doc::text(format!("{}: ", kw(n.name))), self.pattern(p)])
                    })
                    .collect();
                if *rest {
                    entries.push(Doc::text(".."));
                }
                Doc::concat([
                    Doc::text(head),
                    Doc::text("{ "),
                    Doc::join(Doc::text(", "), entries.into_iter()),
                    Doc::text(" }"),
                ])
            }
            Pattern::Record {
                enum_name,
                name,
                fields,
                rest,
            } => {
                let head = match (enum_name, name) {
                    (Some(e), Some(n)) => format!("{}::{} ", kw(e.name), kw(n.name)),
                    (None, Some(n)) => format!("{} ", kw(n.name)),
                    _ => String::new(),
                };
                let mut entries: Vec<Doc> = fields
                    .iter()
                    .map(|(n, p)| {
                        if let Pattern::Ident(id) = p {
                            if id.name == n.name {
                                return Doc::text(kw(n.name));
                            }
                        }
                        Doc::concat([Doc::text(format!("{}: ", kw(n.name))), self.pattern(p)])
                    })
                    .collect();
                if *rest {
                    entries.push(Doc::text(".."));
                }
                Doc::concat([
                    Doc::text(head),
                    Doc::text("{ "),
                    Doc::join(Doc::text(", "), entries),
                    Doc::text(" }"),
                ])
            }
            Pattern::Range {
                start,
                end,
                inclusive,
            } => {
                let dots = if *inclusive { "..=" } else { ".." };
                Doc::concat([
                    start.map(|s| self.expr(s)).unwrap_or_else(nil),
                    Doc::text(dots),
                    end.map(|e| self.expr(e)).unwrap_or_else(nil),
                ])
            }
            Pattern::Or(ps) => {
                Doc::join(Doc::text(" | "), ps.iter().map(|p| self.pattern(p)))
            }
        }
    }

    // ---- trailing comments ------------------------------------------------

    /// Append a trailing same-line comment to `doc` if one exists on the span's
    /// final line and the span is single-line (so it can't misattach to a
    /// nested construct's last line). Alignment is a later post-pass; for now a
    /// fixed two-space gap.
    /// The trailing comment a construct with this span actually claims: the one
    /// on its last source line, IF nothing but whitespace/`;`/`,` sits between
    /// the construct's end and the `//`. When several constructs share a source
    /// line (`if c { a; b }  // cmt`, or `Foo { x }  // cmt`), the inner ones
    /// have a `}` between them and the comment; only the outer one claims it.
    fn claimed_trailing_comment(&self, span: Span) -> Option<(&str, usize)> {
        let (text, col, offset) = self.trivia.trailing_comment(span.line_range.1)?;
        let between = self.slice_range(span.offset_range.1, offset);
        between
            .chars()
            .all(|c| c.is_whitespace() || c == ';' || c == ',')
            .then_some((text, col))
    }

    fn with_trailing_comment(&self, doc: Doc, span: Span, mode: char) -> Doc {
        // The sentinel is appended at the end of `doc`, landing on the rendered
        // last line -- correct whether the construct is one line or several (a
        // call/array the formatter broke across lines).
        match self.claimed_trailing_comment(span) {
            Some((text, col)) => Doc::concat([doc, self.sentinel(text, col, mode)]),
            None => doc,
        }
    }

    /// Build a trailing-comment sentinel `\0<mode><col>\0//text` for the align
    /// post-pass. Mode: `A` (struct fields / variants / match arms) always
    /// aligns the group; `P` (in-block statements) aligns only if the author
    /// already did; `N` (top-level items) never aligns (two-space gap).
    fn sentinel(&self, text: &str, col: usize, mode: char) -> Doc {
        Doc::text(format!(
            "{S}{mode}{col}{S}//{text}",
            S = crate::align::SENTINEL
        ))
    }
}

/// Surface spelling of a non-overloadable binary operator. `&&`/`||` tokenize as
/// `And`/`Or` whose `to_code()` is the keyword form, so special-case them.
fn binop_str(op: &TokenKind) -> String {
    match op {
        TokenKind::And => "&&".to_string(),
        TokenKind::Or => "||".to_string(),
        other => other.to_code(),
    }
}

/// Lowercase a numeric literal's base prefix (`0X`/`0B`/`0O`), preserving the
/// digits' case (`0XFF` -> `0xFF`).
fn normalize_int(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'0' && matches!(bytes[1], b'X' | b'B' | b'O') {
        let mut out = String::with_capacity(s.len());
        out.push('0');
        out.push(bytes[1].to_ascii_lowercase() as char);
        out.push_str(&s[2..]);
        out
    } else {
        s.to_string()
    }
}

/// The separator for an infix operator. The range operator `..` is set tight
/// (`a..b`); everything else gets one space on each side.
fn binop_sep(op: &TokenKind) -> Doc {
    match op {
        TokenKind::DoubleDot => Doc::text(".."),
        other => Doc::text(format!(" {} ", binop_str(other))),
    }
}

/// The modules-v1 visibility modifier as written, with its trailing space;
/// nothing when none was written.
fn item_visibility(vis: Option<&Visibility>) -> Vec<Doc> {
    let text = match vis {
        None => return Vec::new(),
        Some(Visibility::Pub) => "pub ".to_string(),
        Some(Visibility::Private) => "private ".to_string(),
        Some(Visibility::PubSuper) => "pub(super) ".to_string(),
        Some(Visibility::PubPackage) => "pub(package) ".to_string(),
        Some(Visibility::PubIn(path)) => {
            let path: Vec<&str> = path.iter().map(|i| i.name.as_str()).collect();
            format!("pub(in {}) ", path.join("::"))
        }
    };
    vec![Doc::text(text)]
}
