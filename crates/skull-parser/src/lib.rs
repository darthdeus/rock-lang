//! skull-parser: an arena-allocated reimplementation of Rock's surface
//! AST + parser, built on `rock-tokenizer` (lexer) and `rock-core`
//! (spans, diagnostics). Goal: A/B the arena allocation strategy against
//! rock's `Box`-based parser.

pub mod ast;
pub mod clone;
pub mod desugar;
pub mod graft;
pub mod macro_expand;
pub mod parser;
pub mod sexp;

pub use ast::*;
pub use parser::Parser;

pub use ordered_float::OrderedFloat;
pub use rock_core::errors::CompilerErrorKind;
pub use rock_core::source_code::{Dialect, GlobalPolicy, IdentityStorage, PackageTag};
pub use rock_core::{CompilerError, SourceFiles, Span, source_file_from_path_and_contents};
pub use rock_tokenizer::TokenKind;
use rock_tokenizer::Tokenizer;
use ustr::Ustr;

/// Tokenize `source` with rock's tokenizer and parse it into an arena `Ast`.
/// `file` is the source-file identity used for spans/diagnostics.
///
/// The parse is TOTAL: error recovery (for partial parses) records the dropped
/// spans in `Ast.recovery_errors` and the best-effort tree is returned `Ok`
/// regardless (rust-analyzer's model -- the tree always comes back). Only an
/// unrecoverable tokenizer failure is a hard `Err`. Strictness is a DOWNSTREAM
/// policy, not the parser's job: the CLI compile/test paths gate on
/// `recovery_errors` (see `rock-cli` `check_modules` / `parse_user_modules`),
/// while the interactive `Session`/LSP keep the partial tree so every feature
/// (completion, hover, rename, inlay) works on broken code and the recovery
/// errors surface as diagnostics.
pub fn parse_module(source: &str, file: Ustr) -> Result<Ast, CompilerError> {
    parse_module_with_dialect(source, file, Dialect::Legacy)
}

/// [`parse_module`] with the source dialect selected. V1 files enable the
/// modules-v1 grammar (`(expr : Type)` ascription, `::` paths, module
/// items) and reject the legacy infix `expr :: Type` ascription.
pub fn parse_module_with_dialect(
    source: &str,
    file: Ustr,
    dialect: Dialect,
) -> Result<Ast, CompilerError> {
    let tokens = Tokenizer::new(source, file).tokenize()?;
    let mut ast = Parser::new_with_dialect(tokens, source, 0, dialect).parse_module()?;
    ast.file = file;
    Ok(ast)
}

/// [`parse_module`] for a caller that has NO recovery story: a recovered syntax
/// error is an `Err` here rather than a dropped item in an otherwise-`Ok` tree.
///
/// Test fixtures and corpus tools want this one. `parse_module(..).expect(..)`
/// reads like it proves the source parsed, and does not -- a malformed item is
/// silently absent from the AST, so the test runs against a partial workspace
/// and passes for a reason that has nothing to do with its name. That is how a
/// `""" .. """` inline body missing its `=` sat in the JS emitter fixtures:
/// accepted, dropped, and the rejection under test came from a different path.
pub fn parse_module_clean(source: &str, file: Ustr) -> Result<Ast, CompilerError> {
    parse_module_clean_with_dialect(source, file, Dialect::Legacy)
}

/// [`parse_module_clean`] with the source dialect selected.
pub fn parse_module_clean_with_dialect(
    source: &str,
    file: Ustr,
    dialect: Dialect,
) -> Result<Ast, CompilerError> {
    let ast = parse_module_with_dialect(source, file, dialect)?;
    match ast.recovery_errors.first() {
        Some(e) => Err(e.clone()),
        None => Ok(ast),
    }
}

/// Parse exactly ONE top-level item from `source` (the text of a single item),
/// with spans anchored at `(offset, line, column)` -- the absolute position of
/// `source[0]` within the outer file. Used by the incremental driver to re-parse
/// just an edited item instead of the whole file: extract the item's text by its
/// stored span, parse it here, and the resulting AST's spans line up with the
/// rest of the file. `line`/`column` are 0-based (rock-core `Span` convention).
/// `dialect` is the owning file's, so re-parsing one item of a v1 file does not
/// quietly read it as legacy.
pub fn parse_one_item(
    source: &str,
    file: Ustr,
    offset: usize,
    line: usize,
    column: usize,
    dialect: Dialect,
) -> Result<Ast, CompilerError> {
    let tokens = Tokenizer::new_with_offset(source, file, offset, line, column).tokenize()?;
    let mut ast = Parser::new_with_dialect(tokens, source, offset, dialect).parse_single_item()?;
    ast.file = file;
    // Total, like `parse_module`. The incremental fast path inspects
    // `recovery_errors` itself: a body-level recovery (e.g. a stray `)` inside the
    // edited item) makes it bail to a full reparse rather than installing a
    // half-parsed item -- a caller policy, not a parse failure.
    Ok(ast)
}

/// Cook a single already-parsed module in place: run the syntax desugars only.
/// Cross-module macro expansion is a separate whole-workspace step (`cook`)
/// since macros resolve across files.
pub fn cook_single(ast: &mut Ast) {
    desugar::desugar(ast);
}

/// Cook a whole workspace in place: cross-module macro expansion, then
/// per-module syntax desugar. This is the front-end's full "cook" — its output
/// is the AST bone consumes, byte-identical to rock post-expand+desugar.
pub fn cook(modules: &mut [Ast]) {
    // Macro expansion FIRST (bone's order), across the whole workspace — macro
    // defs resolve across files — then per-module syntax desugar.
    macro_expand::expand_macros(modules);
    for ast in modules.iter_mut() {
        desugar::desugar(ast);
    }
}

/// Cook a SINGLE module against a prebuilt `registry`: macro-expand it, then
/// syntax-desugar it. The per-module unit of `cook`, exposed so an incremental
/// driver can re-cook just one edited file -- build the registry once with
/// `collect_registry` from the un-expanded modules, then `cook_one` each file
/// (or only the edited one). Equivalent to that module's slice of `cook` since
/// both expansion and desugar are per-module given a fixed registry.
pub fn cook_one(ast: &mut Ast, registry: &macro_expand::MacroRegistry) {
    macro_expand::expand_one(ast, registry);
    desugar::desugar(ast);
}

pub use macro_expand::{
    MacroRegistry, check_macro_call_labels, check_macro_call_labels_in,
    check_macro_name_uniqueness, collect_registry,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a single-function source and return its body block's s-expr.
    /// The s-expr format is byte-compatible with rock's `Block::to_sexp`,
    /// so these expectations double as the cross-parser contract.
    fn body_sexp(src: &str) -> String {
        let ast = parse_module(src, Ustr::from("test.rock")).expect("parse failed");
        match &ast.items[0].kind {
            ItemKind::Function(f) => ast.block_sexp(f.body.rock_block().unwrap()),
            other => panic!("expected function item, got {:?}", other),
        }
    }

    fn check(body: &str, expected: &str) {
        let src = format!("fn f() {{ {} }}", body);
        assert_eq!(body_sexp(&src), expected, "source: {src:?}");
    }

    /// Parse `fn f() { let x = <body> }` and return the AST + the let's value.
    fn let_value(body: &str) -> (Ast, Idx<ExprData>) {
        let src = format!("fn f() {{ let x = {} }}", body);
        let ast = parse_module(&src, Ustr::from("test.rock")).expect("parse failed");
        let block = match &ast.items[0].kind {
            ItemKind::Function(f) => f.body.rock_block().unwrap(),
            other => panic!("expected fn, got {other:?}"),
        };
        let stmt = ast.blocks[block].statements[0];
        let value = match &ast.stmts[stmt].kind {
            StatementKind::Let(l) => l.value.expect("let has no value"),
            other => panic!("expected let, got {other:?}"),
        };
        (ast, value)
    }

    #[test]
    fn parse_one_item_matches_in_context() {
        // Re-parsing a single item from its text slice (with its absolute
        // offset/line/col) must reproduce EXACTLY what the whole-module parse
        // produced for that item -- same item span, same body-block span, same
        // body s-expr -- so the incremental driver can swap it in. The middle
        // item is multi-line and indented so a wrong base offset/line/col shows.
        let src = "fn a() -> int { 1 }\n\
                   fn b(x: int) -> int {\n    \
                       let y = x + 2\n    \
                       y * 3\n\
                   }\n\
                   fn c() -> int { 9 }\n";
        let file = Ustr::from("test.rock");
        let full = parse_module(src, file).expect("full parse");
        let item = &full.items[1];
        let (s, e) = item.span.offset_range;
        let sub = &src[s..e];

        let mini = parse_one_item(
            sub,
            file,
            s,
            item.span.line_range.0,
            item.span.col_range.0,
            Dialect::Legacy,
        )
        .expect("parse_one_item");
        assert_eq!(mini.items.len(), 1, "exactly one item parsed");
        let m = &mini.items[0];

        assert_eq!(
            m.span.offset_range, item.span.offset_range,
            "item offset span"
        );
        assert_eq!(m.span.line_range, item.span.line_range, "item line range");

        let body_block = |it: &Item| match &it.kind {
            ItemKind::Function(f) => f.body.rock_block().unwrap(),
            other => panic!("expected fn, got {other:?}"),
        };
        let mb = body_block(m);
        let fb = body_block(item);
        // A nested node's absolute span: wrong base offset would shift it.
        assert_eq!(
            mini.b(&mb).span.offset_range,
            full.b(&fb).span.offset_range,
            "body block span"
        );
        assert_eq!(mini.block_sexp(mb), full.block_sexp(fb), "body s-expr");
    }

    #[test]
    fn parse_one_item_annotated() {
        // item.span covers leading annotations -- the slice must start at `@`.
        let src = "fn pre() {}\n@test\nfn tagged() -> int { 7 }\n";
        let file = Ustr::from("test.rock");
        let full = parse_module(src, file).expect("full parse");
        let item = &full.items[1];
        let (s, e) = item.span.offset_range;
        let mini = parse_one_item(
            &src[s..e],
            file,
            s,
            item.span.line_range.0,
            item.span.col_range.0,
            Dialect::Legacy,
        )
        .expect("parse_one_item");
        assert_eq!(
            mini.items[0].span.offset_range, item.span.offset_range,
            "annotated item span"
        );
        assert_eq!(mini.items[0].annotations.len(), 1, "annotation preserved");
    }

    #[test]
    fn parse_module_clean_rejects_what_parse_module_recovers_from() {
        // A fn body-less because its inline body lost its `=`: recovered, the
        // item dropped, and `parse_module` still Ok. The strict form is the
        // oracle a fixture helper needs.
        let src = "inline(lua) fn f(x: int) -> int \"\"\" return x \"\"\"\n";
        let file = Ustr::from("<clean>");
        let lenient = parse_module(src, file).expect("recovering parse still returns a tree");
        assert!(
            !lenient.recovery_errors.is_empty(),
            "the dropped item is recorded"
        );
        assert!(
            !lenient
                .items
                .iter()
                .any(|i| matches!(&i.kind, ItemKind::Function(_))),
            "the malformed fn is absent from the lenient tree, which is the trap"
        );
        assert!(
            parse_module_clean(src, file).is_err(),
            "strict parse rejects it"
        );
        // Both sides: clean source must still come back Ok.
        assert!(parse_module_clean("fn f(x: int) -> int { return x }\n", file).is_ok());
    }

    #[test]
    fn nesting_beyond_the_limit_drops_only_its_item() {
        let depth = rock_core::nesting::MAX_NESTING_DEPTH as usize;
        let deep = format!(
            "fn deep() {{\n{}{}}}\n",
            "if true {\n".repeat(depth),
            "}\n".repeat(depth)
        );
        let src = format!("{deep}fn after() {{\n    let broken =\n    let kept = 1\n}}\n");
        // The parser recurses to the limit, so it needs the stack compiler threads get.
        let ast = std::thread::Builder::new()
            .stack_size(rock_core::nesting::COMPILER_STACK_BYTES)
            .spawn(move || parse_module(&src, Ustr::from("<deep>")).expect("tokenization"))
            .unwrap()
            .join()
            .unwrap();
        let messages: Vec<&str> = ast
            .recovery_errors
            .iter()
            .map(|e| e.message.as_str())
            .collect();
        assert_eq!(
            messages.len(),
            2,
            "one error for the deep item, one for the broken let: {messages:?}"
        );
        assert!(messages[0].contains("nested too deeply"), "{messages:?}");
        let names: Vec<_> = ast
            .items
            .iter()
            .filter_map(|i| match &i.kind {
                ItemKind::Function(f) => Some(f.name.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            names,
            ["after"],
            "the deep fn is dropped and statement recovery still works after it"
        );
    }

    #[test]
    fn missing_generic_close_is_recorded_as_recovery() {
        let src = "struct Box<T { value: int }\n";
        let ast = parse_module(src, Ustr::from("<generic-close>")).expect("tokenization");
        assert!(
            !ast.recovery_errors.is_empty(),
            "a missing generic close must not become a clean struct"
        );
    }

    fn first_fn(ast: &Ast) -> &FunctionDef {
        match &ast.items[0].kind {
            ItemKind::Function(f) => f,
            other => panic!("expected fn, got {other:?}"),
        }
    }

    fn first_struct(ast: &Ast) -> &StructDef {
        match &ast.items[0].kind {
            ItemKind::Struct(s) => s,
            other => panic!("expected struct, got {other:?}"),
        }
    }

    #[test]
    fn param_annotation_parses_with_arg_and_span() {
        // `@size(4)` on the first param; the second is unannotated. Name, arg
        // value, and the annotation's own span must all survive.
        let src = "fn f(@size(4) a: int, b: float) {}";
        let ast = parse_module(src, Ustr::from("test.rock")).expect("parse failed");
        assert!(
            ast.recovery_errors.is_empty(),
            "clean parse: {:?}",
            ast.recovery_errors
        );
        let f = first_fn(&ast);
        assert_eq!(f.params[0].annotations.len(), 1, "param 0 annotated");
        assert_eq!(f.params[1].annotations.len(), 0, "param 1 unannotated");
        let a = &f.params[0].annotations[0];
        assert_eq!(a.name.name, "size");
        assert_eq!(a.args.len(), 1, "one arg");
        assert!(a.args[0].name.is_none(), "positional arg");
        assert!(
            matches!(&ast.exprs[a.args[0].value].kind, ExprKind::Int(4, _)),
            "arg value is int 4, got {:?}",
            ast.exprs[a.args[0].value].kind
        );
        // The annotation span is a real, non-empty source range distinct from
        // the param name's.
        assert!(
            a.span.offset_range.0 < a.span.offset_range.1,
            "anno span non-empty"
        );
        assert_ne!(
            a.span.offset_range, f.params[0].name.span.offset_range,
            "anno span != name span"
        );
    }

    #[test]
    fn multiple_annotations_on_one_param() {
        let src = "fn f(@instance @size(2) x: int) {}";
        let ast = parse_module(src, Ustr::from("test.rock")).expect("parse failed");
        assert!(
            ast.recovery_errors.is_empty(),
            "clean parse: {:?}",
            ast.recovery_errors
        );
        let anns = &first_fn(&ast).params[0].annotations;
        assert_eq!(anns.len(), 2, "two annotations");
        assert_eq!(anns[0].name.name, "instance");
        assert!(anns[0].args.is_empty());
        assert_eq!(anns[1].name.name, "size");
        assert_eq!(anns[1].args.len(), 1);
    }

    #[test]
    fn field_annotation_parses_with_arg() {
        // `@flat` bare and `@w(3)` with an arg on struct fields.
        let src = "struct S { @flat a: int, @w(3) b: float, c: bool }";
        let ast = parse_module(src, Ustr::from("test.rock")).expect("parse failed");
        assert!(
            ast.recovery_errors.is_empty(),
            "clean parse: {:?}",
            ast.recovery_errors
        );
        let s = first_struct(&ast);
        assert_eq!(s.fields[0].annotations.len(), 1, "field 0 annotated");
        assert_eq!(s.fields[0].annotations[0].name.name, "flat");
        assert!(s.fields[0].annotations[0].args.is_empty());
        assert_eq!(s.fields[1].annotations.len(), 1, "field 1 annotated");
        assert_eq!(s.fields[1].annotations[0].name.name, "w");
        assert_eq!(s.fields[1].annotations[0].args.len(), 1);
        assert_eq!(s.fields[2].annotations.len(), 0, "field 2 unannotated");
    }

    #[test]
    fn multiple_annotations_on_value_field() {
        let src = "value V { @flat @w c: int }";
        let ast = parse_module(src, Ustr::from("test.rock")).expect("parse failed");
        assert!(
            ast.recovery_errors.is_empty(),
            "clean parse: {:?}",
            ast.recovery_errors
        );
        let s = first_struct(&ast);
        assert!(s.is_value, "value struct");
        let anns = &s.fields[0].annotations;
        assert_eq!(anns.len(), 2, "two field annotations");
        assert_eq!(anns[0].name.name, "flat");
        assert_eq!(anns[1].name.name, "w");
    }

    #[test]
    fn nobox_value_marks_the_value_type() {
        let src = "nobox value V { x: int }";
        let ast = parse_module(src, Ustr::from("test.rock")).expect("parse failed");
        assert!(
            ast.recovery_errors.is_empty(),
            "clean parse: {:?}",
            ast.recovery_errors
        );
        let s = first_struct(&ast);
        assert!(s.is_value);
        assert!(s.is_nobox);
    }

    #[test]
    fn nobox_rejects_a_reference_struct() {
        let ast = parse_module("nobox struct S { x: int }", Ustr::from("test.rock"))
            .expect("parser recovers the invalid item");
        assert!(
            ast.recovery_errors
                .iter()
                .any(|err| err.message.contains("`nobox` requires `value`")),
            "{:?}",
            ast.recovery_errors
        );
    }

    #[test]
    fn item_and_param_annotations_coexist() {
        // An item-level annotation on the fn plus a param annotation must both
        // land -- existing item-annotation parsing is untouched.
        let src = "@pure fn f(@size(1) a: int) {}";
        let ast = parse_module(src, Ustr::from("test.rock")).expect("parse failed");
        assert!(
            ast.recovery_errors.is_empty(),
            "clean parse: {:?}",
            ast.recovery_errors
        );
        assert_eq!(
            ast.items[0].annotations.len(),
            1,
            "item annotation preserved"
        );
        assert_eq!(ast.items[0].annotations[0].name.name, "pure");
        assert_eq!(
            first_fn(&ast).params[0].annotations.len(),
            1,
            "param annotation preserved"
        );
    }

    #[test]
    fn unannotated_params_and_fields_stay_empty() {
        // Regression guard: the new slots default empty for ordinary decls.
        let src = "fn f(a: int, b: float) {} struct S { x: int } value V { y: float }";
        let ast = parse_module(src, Ustr::from("test.rock")).expect("parse failed");
        assert!(
            ast.recovery_errors.is_empty(),
            "clean parse: {:?}",
            ast.recovery_errors
        );
        let f = first_fn(&ast);
        assert!(
            f.params.iter().all(|p| p.annotations.is_empty()),
            "no param annotations"
        );
        for it in &ast.items[1..] {
            if let ItemKind::Struct(s) = &it.kind {
                assert!(
                    s.fields.iter().all(|fl| fl.annotations.is_empty()),
                    "no field annotations"
                );
            }
        }
    }

    #[test]
    fn interpolation_keeps_static_format_spec() {
        // `"{v:.3}"` is a single embedded expr — must carry precision=3.
        let (ast, v) = let_value(r#""{v:.3}""#);
        let ExprKind::InterpolatedString(parts) = &ast.exprs[v].kind else {
            panic!("expected interpolated string, got {:?}", ast.exprs[v].kind);
        };
        let InterpolatedPart::Expr(_, spec) = &parts[0] else {
            panic!("expected embedded expr part");
        };
        let spec = spec.as_ref().expect("format spec dropped");
        assert_eq!(spec.precision, Some(3));
    }

    #[test]
    fn interpolation_keeps_dynamic_width_and_precision_exprs() {
        // `{v:{w}.{p}f}` — the embedded width/precision exprs must survive as
        // real arena nodes (this is the data rock keeps and the cook oracle
        // cannot see).
        let (ast, v) = let_value(r#""{v:{w}.{p}f}""#);
        let ExprKind::InterpolatedString(parts) = &ast.exprs[v].kind else {
            panic!("expected interpolated string");
        };
        let InterpolatedPart::Expr(_, spec) = &parts[0] else {
            panic!("expected embedded expr part");
        };
        let spec = spec.as_ref().expect("format spec dropped");
        let we = spec.width_expr.expect("width_expr dropped");
        let pe = spec.precision_expr.expect("precision_expr dropped");
        assert!(matches!(&ast.exprs[we].kind, ExprKind::Ident(id) if id.name == "w"));
        assert!(matches!(&ast.exprs[pe].kind, ExprKind::Ident(id) if id.name == "p"));
        assert_eq!(spec.format_type, Some('f'));
    }

    #[test]
    fn match_arm_guard_does_not_swallow_the_arrow() {
        // A bare-ident guard `if x` must not parse as a `x => {}` lambda and eat
        // the arm's `=>`. Before the `arrow_lambda_blocked` fix this collapsed the
        // whole match (recovered to a stray `E::B {}`).
        let s = body_sexp("fn f(e: E, x: bool) { match e { E::A if x => {} E::B => {} } }");
        assert_eq!(
            s, "(block (match e (arm E::A (if x) (block )) (arm E::B (block ))))",
            "guarded arm must parse as a guard, not a lambda"
        );
        // Paren-form guard `(x)` and binop guards stay guards too (the arm's `=>`
        // survives, the match isn't collapsed).
        let p = body_sexp("fn f(e: E, x: bool) { match e { E::A if (x) => {} _ => {} } }");
        assert!(
            p.contains("(arm E::A (if") && p.contains("(arm _"),
            "paren-form guard: {p}"
        );
        let b = body_sexp("fn f(e: E, x: bool) { match e { E::A if x && x => {} _ => {} } }");
        assert!(b.contains("(arm E::A (if (and x x))"), "binop guard: {b}");
    }

    #[test]
    fn match_arm_body_allows_a_struct_literal() {
        // `=> S { .. }` in a match arm is a struct literal (Rust parity), not the
        // ident `S` then a block. Pre-fix `with_allow_literals(false)` on the arm
        // body stopped the expr at `S`, leaving `{ x: 1.0 }` to mis-parse as a new
        // arm pattern ("unsupported pattern: OpenCurly").
        let s =
            body_sexp("fn f(k: K) -> S { match k { K::A => S { x: 1.0 } K::B => S { x: 2.0 } } }");
        assert!(
            s.contains("(arm K::A (block (struct S"),
            "arm body must parse as a struct literal, not stop at the ident: {s}"
        );
    }

    #[test]
    fn struct_literal_recovers_a_missing_comma_between_fields() {
        // A field value not followed by `,` but by another `field:` is a
        // missing-comma typo, not the end of the literal -- recover (record the
        // error, keep parsing) so the whole literal survives. This is what lets
        // completion inside an unterminated value above the next field see the
        // literal at all (`z: st.<cursor>`).
        let s = body_sexp("fn f() { let p = P { a: 1 b: 2 } }");
        assert!(
            s.contains("(struct P") && s.contains("a 1") && s.contains("b 2"),
            "both fields must survive a missing comma: {s}"
        );
    }

    #[test]
    fn parse_static_local_inferred() {
        // `static count = 0` -> `(static <name> <ty-or-()> <value>)`, mirroring
        // the `(let ...)` shape. Name is a plain ident (no destructuring); the
        // initializer is mandatory.
        check("static count = 0", "(block (static count () 0))");
    }

    #[test]
    fn parse_static_local_annotated() {
        check(
            "static scratch: [FixVec2] = []",
            "(block (static scratch [FixVec2] []))",
        );
    }

    #[test]
    fn parse_static_is_not_a_hard_keyword() {
        // `static` used as an ordinary ident (a call arg) stays an ident -- the
        // contextual keyword only fires at statement start followed by an ident.
        let s = body_sexp(r#"fn f() { load_audio("a.wav", static) }"#);
        assert!(
            s.contains("static"),
            "static usable as an ordinary ident: {s}"
        );
    }

    #[test]
    fn lambda_still_parses_outside_a_guard() {
        // The block must not leak into normal lambda parsing.
        let s = body_sexp("fn f() { let g = x => x + 1 }");
        assert!(
            s.contains("lambda") || s.contains("=>"),
            "bare-ident lambda still parses: {s}"
        );
    }

    #[test]
    fn item_recovery_does_not_swallow_the_next_fn() {
        // An item error detected AT an item boundary leaves the parser sitting on
        // the next `fn`. Recovery used to force-consume a token before scanning,
        // so that `fn` was eaten and the whole following function vanished from
        // the AST -- a valid item lost to a neighbour's error, which in the editor
        // took hover/completion down with it.
        let src = "fn helper(a: int) -> int { return a * 2 }\n\
                   fn incomplete(x: int) -> int\n\
                   fn survivor() -> int { return 7 }\n";
        let ast = parse_module(src, Ustr::from("<recovery>")).expect("total parse");
        assert_eq!(
            ast.recovery_errors.len(),
            1,
            "exactly the one malformed item"
        );
        let fns: Vec<&str> = ast
            .items
            .iter()
            .filter_map(|i| match &i.kind {
                ItemKind::Function(f) => Some(f.name.name.as_str()),
                _ => None,
            })
            .collect();
        assert!(fns.contains(&"helper"), "the item before survives: {fns:?}");
        assert!(
            fns.contains(&"survivor"),
            "the item AFTER survives: {fns:?}"
        );
        assert!(
            !fns.contains(&"incomplete"),
            "the malformed item is the only one dropped: {fns:?}"
        );
    }

    #[test]
    fn expr_position_hole_is_represented() {
        let (ast, v) = let_value("?");
        assert!(matches!(ast.exprs[v].kind, ExprKind::Hole(_, None)));
        let (ast, v) = let_value("?foo");
        assert!(matches!(&ast.exprs[v].kind, ExprKind::Hole(_, Some(n)) if n == "foo"));
    }

    #[test]
    fn type_hole_keeps_name() {
        let src = "fn f() { let x: ?foo = 1 }";
        let ast = parse_module(src, Ustr::from("test.rock")).expect("parse failed");
        let block = match &ast.items[0].kind {
            ItemKind::Function(f) => f.body.rock_block().unwrap(),
            _ => unreachable!(),
        };
        let stmt = ast.blocks[block].statements[0];
        let StatementKind::Let(l) = &ast.stmts[stmt].kind else {
            panic!("expected let")
        };
        let ty = l.ty.expect("no type annotation");
        assert!(matches!(&ast.types[ty].kind, TypeExprKind::Hole(_, Some(n)) if n == "foo"));
    }

    #[test]
    fn block_recovers_after_empty_rhs_let() {
        // `let invalid_syntax =` with no RHS must not drop the whole fn body:
        // statement-level recovery keeps the lets before AND after the error so
        // scope-based features (completion) still see the locals. Mirrors
        // tests/lsp/completion/basic.rock.lsp::complete_local_vars_with_syntax_error.
        let src = "fn calculate() {\n\
            \x20   let first_value = 10\n\
            \x20   let second_value = 20\n\
            \x20   let invalid_syntax = \n\
            \n\
            \x20   let result = fi\n\
            }";
        // Total parse: the malformed `let invalid_syntax =` does NOT make
        // `parse_module` fail -- the best-effort tree comes back `Ok`, with the
        // dropped span recorded in `recovery_errors` (a downstream compile gate
        // turns that into a rejection; IDE features keep the tree). Not swallowed.
        let ast = parse_module(src, Ustr::from("test.rock")).expect("parse is total");
        assert!(
            !ast.recovery_errors.is_empty(),
            "the dropped `let` must be recorded in recovery_errors",
        );
        let block = match &ast.items[0].kind {
            ItemKind::Function(f) => f.body.rock_block().unwrap(),
            other => panic!("expected fn, got {other:?}"),
        };
        // The malformed `let invalid_syntax =` is dropped (its RHS never parsed),
        // but the three well-formed lets around it survive in order.
        assert_eq!(
            ast.block_sexp(block),
            "(block (let first_value () 10) (let second_value () 20) (let result () fi))",
        );
    }

    #[test]
    fn format_spec_is_invisible_to_the_sexp_oracle() {
        // The captured spec must NEVER leak into the s-expr — a spec-bearing
        // interpolation renders byte-identically to its spec-less form, so
        // cook-diff parity with rock is preserved.
        assert_eq!(
            body_sexp(r#"fn f() { let x = "{v:.3}" }"#),
            body_sexp(r#"fn f() { let x = "{v}" }"#),
        );
        assert_eq!(
            body_sexp(r#"fn f() { let x = "a {v:>10} b" }"#),
            body_sexp(r#"fn f() { let x = "a {v} b" }"#),
        );
        // Dynamic width/precision: the `{w}`/`{p}` exprs are stored on the spec,
        // not rendered as separate parts.
        assert_eq!(
            body_sexp(r#"fn f() { let x = "{v:{w}.{p}f}" }"#),
            body_sexp(r#"fn f() { let x = "{v}" }"#),
        );
    }

    #[test]
    fn interpolation_hole_colon_inside_brackets_is_not_a_format_spec() {
        // A `:` nested inside `()`/`{}`/`[]` in the hole belongs to the
        // expression, not the `:spec` split -- named args, struct literals,
        // etc. must survive. The split only fires at bracket-depth 0.
        // Named-arg call:
        assert_eq!(
            body_sexp(r#"fn f() { let x = "{add2(2, b: 5)}" }"#),
            r#"(block (let x () "(call add2 2 b: 5)"))"#,
        );
        // Struct literal (colon AND the closing `}` are both depth>0):
        assert_eq!(
            body_sexp(r#"fn f() { let x = "{P { v: 1 }}" }"#),
            r#"(block (let x () "(struct P (data v 1))"))"#,
        );
        // Nested-call named arg deeper in:
        assert_eq!(
            body_sexp(r#"fn f() { let x = "{g(h(a: 1), b: 2)}" }"#),
            r#"(block (let x () "(call g (call h a: 1) b: 2)"))"#,
        );
    }

    #[test]
    fn interpolation_hole_string_literal_brackets_and_colons_are_opaque() {
        // A string literal inside the hole is opaque: its brackets must not move
        // the bracket-depth counter and its `:` must not trigger the spec split.
        // Regression guard -- a naive char-level depth counter miscounts `(`/`[`
        // inside inner strings (broke tests/strings/characterization split cases).
        assert_eq!(
            body_sexp(r#"fn f() { let x = "{join("a[b]c".split("["), "|")}" }"#),
            r#"(block (let x () "(call join (call (. "a[b]c" split) "[") "|")"))"#,
        );
        // A `:` inside an inner string is text, not a format-spec separator.
        assert_eq!(
            body_sexp(r#"fn f() { let x = "{greet("a:b")}" }"#),
            r#"(block (let x () "(call greet "a:b")"))"#,
        );
    }

    #[test]
    fn interpolation_depth0_colon_is_still_a_format_spec() {
        // The fix must NOT break real format specs: a `:` at bracket-depth 0
        // still splits expr from spec, and a bracketed expr followed by a
        // depth-0 spec keeps both.
        assert_eq!(
            body_sexp(r#"fn f() { let x = "{v:.3}" }"#),
            body_sexp(r#"fn f() { let x = "{v}" }"#),
        );
        // `xs[0]` closes its bracket, THEN `:3` is a genuine width spec.
        let (ast, v) = let_value(r#""{xs[0]:3}""#);
        let ExprKind::InterpolatedString(parts) = &ast.exprs[v].kind else {
            panic!("expected interpolated string, got {:?}", ast.exprs[v].kind);
        };
        let InterpolatedPart::Expr(e, spec) = &parts[0] else {
            panic!("expected embedded expr part");
        };
        assert!(
            matches!(&ast.exprs[*e].kind, ExprKind::Index(..)),
            "expr is the index, not truncated"
        );
        assert_eq!(spec.as_ref().expect("width spec dropped").width, Some(3));
    }

    #[test]
    fn interpolation_malformed_hole_is_rejected_not_emitted_literally() {
        // A hole whose expression genuinely fails to parse must surface a
        // spanned error, never silently fall back to literal brace text
        // ("reject, don't fall back"). `add2(2,` is an unterminated call.
        let ast = parse_module(r#"fn f() { let x = "{add2(2,}" }"#, Ustr::from("test.rock"));
        let rejected = match ast {
            Err(_) => true,
            Ok(a) => !a.recovery_errors.is_empty(),
        };
        assert!(
            rejected,
            "malformed hole must be rejected, not emitted as literal text"
        );
    }

    /// Every `Ident` span in `ast`, as (name, the source text it covers).
    fn ident_spans(ast: &Ast, src: &str) -> Vec<(String, String)> {
        ast.exprs
            .iter()
            .filter_map(|(_, x)| match &x.kind {
                ExprKind::Ident(id) => {
                    let (s, e) = x.span.offset_range;
                    Some((
                        id.name.to_string(),
                        src.get(s..e).unwrap_or("<out of range>").to_string(),
                    ))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn interpolation_nested_hole_spans_are_file_offsets() {
        // A hole inside a string that itself sits in a hole is scanned against
        // the enclosing hole's cooked text; its offsets must still come out in
        // file coordinates. The escaped form cooks `\"` away, so its map is not
        // a plain shift.
        let src = "fn a() {}\n\
                   fn b(y: int, z: int) -> string {\n    \
                       let s = \"v {y} w {if true { \"{z}\" } else { \"\" }}\"\n    \
                       \"{s} {f(\\\"n {h(y, g(\\\"{z}\\\"))}\\\")}\"\n\
                   }\n";
        let file = Ustr::from("test.rock");
        let full = parse_module(src, file).expect("full parse");
        for (name, text) in ident_spans(&full, src) {
            assert_eq!(text, name, "ident span does not cover its source");
        }

        // The single-item reparse starts its source mid-file, so a hole offset
        // left relative to that slice lands `offset` bytes short.
        let item = &full.items[1];
        let (s, e) = item.span.offset_range;
        let mini = parse_one_item(
            &src[s..e],
            file,
            s,
            item.span.line_range.0,
            item.span.col_range.0,
            Dialect::Legacy,
        )
        .expect("parse_one_item");
        for (name, text) in ident_spans(&mini, src) {
            assert_eq!(text, name, "reparsed ident span does not cover its source");
        }
    }

    #[test]
    fn expr_hole_sexp_matches_rock_numbering() {
        // Bare anonymous hole renders `?0`; a named one renders `?name`.
        assert_eq!(body_sexp("fn f() { let x = ? }"), "(block (let x () ?0))");
        assert_eq!(
            body_sexp("fn f() { let x = ?foo }"),
            "(block (let x () ?foo))"
        );
        // A type hole bumps the shared counter, so the following expr hole is ?1.
        assert_eq!(
            body_sexp("fn f() { let a: ? = 1\nlet b = ? }"),
            "(block (let a ? 1) (let b () ?1))"
        );
    }

    #[test]
    fn arithmetic_precedence() {
        check("1 + 2 * 3", "(block (+ 1 (* 2 3)))");
        check("1 * 2 + 3", "(block (+ (* 1 2) 3))");
        check("-x + 1", "(block (+ (- x) 1))");
    }

    #[test]
    fn comparisons_and_logic() {
        check("a == b", "(block (== a b))");
        check("a < b && c > d", "(block (and (< a b) (> c d)))");
    }

    #[test]
    fn keyword_spans_and_dropped_fields_are_captured() {
        // Data rock carries that the s-expr can't show — must survive parse.
        let ast = parse_module(
            "global G: int = 0\nfn f() {\nfor x: int in xs {\nreturn x\n}\n}",
            Ustr::from("t.rock"),
        )
        .expect("parse failed");

        let ItemKind::Global(g) = &ast.items[0].kind else {
            panic!("expected global")
        };
        assert!(g.keyword_span.is_some(), "global keyword_span dropped");

        // fn keyword_span, the `for x: T in` element type, and `return` keyword_span.
        let ItemKind::Function(func) = &ast.items[1].kind else {
            panic!("expected fn")
        };
        assert!(func.keyword_span.is_some(), "fn keyword_span dropped");
        let body = func.body.rock_block().unwrap();
        let for_stmt = ast.blocks[body].statements[0];
        let StatementKind::For(f) = &ast.stmts[for_stmt].kind else {
            panic!("expected for")
        };
        assert!(f.keyword_span.is_some(), "for keyword_span dropped");
        let ty = f.ty.expect("for element type `: int` dropped");
        assert!(matches!(ast.types[ty].kind, TypeExprKind::Int));
        let ret = ast.blocks[f.body].statements[0];
        let StatementKind::Return { keyword_span, .. } = &ast.stmts[ret].kind else {
            panic!("expected return");
        };
        assert!(keyword_span.is_some(), "return keyword_span dropped");
    }

    #[test]
    fn ref_type_is_a_prefix_type_former() {
        let ast = parse_module_clean(
            "fn use_player(player: ref Player, maybe: (ref Player)?) -> ref Player { player }",
            Ustr::from("t.rock"),
        )
        .expect("ref types parse");
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn")
        };

        assert_eq!(ast.type_surface(f.params[0].ty), "ref Player");
        assert_eq!(ast.type_surface(f.params[1].ty), "(ref Player)?");
        assert_eq!(
            ast.type_surface(f.return_ty.expect("return type")),
            "ref Player"
        );
    }

    #[test]
    fn sibling_field_ref_in_default() {
        // `field: T = .sibling` — leading-dot sibling ref. No corpus file
        // exercises this, so the cook oracle can't catch a parser gap here;
        // renders `.name`.
        let ast = parse_module(
            "struct HealthBar { max: int, current: int = .max }",
            Ustr::from("t.rock"),
        )
        .expect("parse failed");
        let ItemKind::Struct(s) = &ast.items[0].kind else {
            panic!("expected struct")
        };
        let def = s.fields[1].default.expect("current has a default");
        assert!(matches!(&ast.exprs[def].kind, ExprKind::SiblingFieldRef(id) if id.name == "max"));
        assert_eq!(ast.expr_sexp(def), ".max");
    }

    #[test]
    fn calls_fields_index() {
        check("foo(1, 2)", "(block (call foo 1 2))");
        check("a.b.c", "(block (. (. a b) c))");
        check("a.b(c)", "(block (call (. a b) c))");
        check("xs[0]", "(block (index xs 0))");
        check("foo()", "(block (call foo))");
        check("draw(x: 1, y: 2)", "(block (call draw x: 1 y: 2))");
    }

    #[test]
    fn struct_spread_call_and_update() {
        // (A) call-site spread `..expr` -- renders `..operand` in the arg list,
        // may follow positional args and precede named overrides.
        check("draw(img, ..props)", "(block (call draw img ..props))");
        check(
            "draw(img, ..props, color: c)",
            "(block (call draw img ..props color: c))",
        );
        // Multiple disjoint spreads.
        check("draw(..a, ..b)", "(block (call draw ..a ..b))");
        // A spread after a named arg is legal (bone rejects only on collision).
        check("draw(x: 1, ..props)", "(block (call draw x: 1 ..props))");

        // (B) functional update -- bases render before fields.
        check(
            "let x = UiRect { ..r, z: 2 }",
            "(block (let x () (struct UiRect (bases ..r) (data z 2))))",
        );
        // Multiple bases.
        check(
            "let x = P { ..a, ..b, c: 1 }",
            "(block (let x () (struct P (bases ..a ..b) (data c 1))))",
        );
    }

    #[test]
    fn struct_spread_parse_errors() {
        // A broken statement recovers to `recovery_errors` rather than a hard
        // `Err`, so a parse "fails" if either surfaces.
        let fails = |body: &str| {
            let src = format!("fn f() {{ {body} }}");
            match parse_module(&src, Ustr::from("test.rock")) {
                Ok(ast) => !ast.recovery_errors.is_empty(),
                Err(_) => true,
            }
        };
        // Field before base in a functional-update literal.
        assert!(fails("let x = P { a: 1, ..base }"), "field before base");
        // Functional update on an enum variant is not supported.
        assert!(fails("let x = E::V { ..base, a: 1 }"), "enum-variant base");
    }

    #[test]
    fn literals_and_arrays() {
        check("[1, 2, 3]", "(block [1 2 3])");
        check("\"hi\"", "(block \"hi\")");
        check("true", "(block true)");
        check("null", "(block null)");
        check("Color::Red", "(block Color::Red)");
    }

    #[test]
    fn statements() {
        check("let x = 5", "(block (let x () 5))");
        check("let x: int = 5", "(block (let x int 5))");
        check("let xs: [IVec2] = a", "(block (let xs [IVec2] a))");
        check("return a + b", "(block (return (+ a b)))");
        check("x = y", "(block (= x y))");
        check("x += 1", "(block (+= x 1))");
    }

    #[test]
    fn if_expression() {
        check(
            "if a { 1 } else { 2 }",
            "(block (if (cond a (block 1)) (else (block 2))))",
        );
        check(
            "if a { 1 } else if b { 2 }",
            "(block (if (cond a (block 1)) (cond b (block 2)) ))",
        );
    }

    #[test]
    fn if_let() {
        check(
            "if let x = foo() { 1 }",
            "(block (if-let x (call foo) (block 1)))",
        );
        check(
            "if let Some(v) = lookup(k) { v } else { 0 }",
            "(block (if-let (Some v) (call lookup k) (block v) (else (block 0))))",
        );
    }

    #[test]
    fn while_let_statement() {
        check(
            "while let x = next() { use(x) }",
            "(block (while-let x (call next) (block (call use x))))",
        );
    }

    #[test]
    fn at_operator() {
        // `f(args) @ rhs` -> `f(args, () => rhs)`, rendered as a normal call.
        check(
            "profile(\"x\") @ bar()",
            "(block (call profile \"x\" (lambda () None (block (call bar)))))",
        );
        check(
            "with_scope(s) @ { foo() }",
            "(block (call with_scope s (lambda () None (block (call foo)))))",
        );
        // Non-call left becomes a call with the lambda as its only arg.
        check(
            "guard @ work()",
            "(block (call guard (lambda () None (block (call work)))))",
        );
    }

    #[test]
    fn fn_type_optional_arrow() {
        // `fn()` param type has an implicit unit return; `(fn())?` is a nullable
        // parenthesized fn type (single-paren unwrap).
        check("let f: fn() = g", "(block (let f (fn () unit) g))");
        check("let f: (fn())? = g", "(block (let f (fn () unit)? g))");
    }

    #[test]
    fn nested_generic_close() {
        // `>>` tokenizes as RightShift; the inner generic close must split it.
        check(
            "let m: Map<int, Vec<int>> = a",
            "(block (let m (Map int (Vec int)) a))",
        );
    }

    #[test]
    fn explicit_generic_call_preserves_type_arguments() {
        assert_eq!(
            body_sexp("fn f() { workgroup_array<uint>(64) }"),
            "(block (call (generic workgroup_array uint) 64))",
        );
    }

    #[test]
    fn leading_operator_continues_across_newline() {
        // A binary op on a fresh line continues the expression, even inside a
        // `{ }` block. Minus follows the same rule as plus once there is a
        // left operand.
        let src = "fn f() {\n    foo(a\n        + b\n        + c)\n}";
        let ast = parse_module_clean(src, Ustr::from("test.rock")).unwrap();
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!()
        };
        assert_eq!(
            ast.block_sexp(f.body.rock_block().unwrap()),
            "(block (call foo (+ (+ a b) c)))"
        );

        let src = "fn f() {\n    let x = 10.0\n        - 1.0\n    return x\n        - 2.0\n}";
        let ast = parse_module_clean(src, Ustr::from("test.rock")).unwrap();
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!()
        };
        assert_eq!(
            ast.block_sexp(f.body.rock_block().unwrap()),
            "(block (let x () (- 10 1)) (return (- x 2)))"
        );

        let src = "fn f() {\n    abs(a\n        - b)\n    pair(a,\n        -b)\n}";
        let ast = parse_module_clean(src, Ustr::from("test.rock")).unwrap();
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!()
        };
        assert_eq!(
            ast.block_sexp(f.body.rock_block().unwrap()),
            "(block (call abs (- a b)) (call pair a (- b)))"
        );
    }

    /// Parse a single-item module and return (item-0 fn name, body s-expr).
    fn item0(src: &str) -> (String, String) {
        let ast = parse_module(src, Ustr::from("test.rock")).expect("parse failed");
        match &ast.items[0].kind {
            ItemKind::Function(f) => (
                f.name.name.to_string(),
                ast.block_sexp(f.body.rock_block().unwrap()),
            ),
            other => panic!("expected function item, got {:?}", other),
        }
    }

    /// Whole-item s-expr of the first item (any kind), via `item_sexp`.
    fn item_sexp(src: &str) -> String {
        let ast = parse_module(src, Ustr::from("test.rock")).expect("parse failed");
        ast.item_sexp(&ast.items[0])
    }

    /// Cook a self-contained module and return its non-macro items' s-expr.
    fn cooked_items(src: &str) -> String {
        let mut ast = parse_module(src, Ustr::from("t.rock")).expect("parse failed");
        cook(std::slice::from_mut(&mut ast));
        ast.items
            .iter()
            .filter(|it| !matches!(&it.kind, ItemKind::Function(f) if f.is_macro))
            .map(|it| ast.item_sexp(it))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn parse_nested_fn_basic() {
        // `fn inc(x: int) -> int { x + 1 }` inside a body -> a `(fn ...)` statement.
        check(
            "fn inc(x: int) -> int { x + 1 }",
            "(block (fn inc ((x int)) int (block (+ x 1))))",
        );
    }

    #[test]
    fn parse_nested_fn_no_return_type() {
        check(
            "fn bump() { n = n + 1 }",
            "(block (fn bump () () (block (= n (+ n 1)))))",
        );
    }

    #[test]
    fn parse_fn_is_not_a_hard_local_keyword() {
        // `fn` as a call arg / value context stays whatever the expression parser
        // makes of it; the nested-fn form only fires at statement start.
        let s = body_sexp("fn outer() { fn inner() -> int { 7 }  let r = inner() }");
        assert!(s.contains("(fn inner"), "nested fn parsed: {s}");
    }

    #[test]
    fn item_macro_splice() {
        // A macro body's items splice into the module at the invocation site.
        assert_eq!(
            cooked_items("macro emit() { fn made() -> int { 42 } }\nemit()"),
            "(fn made () int (block 42))"
        );
    }

    #[test]
    fn item_macro_ty_fragment() {
        // `ty` fragment substitutes into a field type position.
        assert_eq!(
            cooked_items("macro hold(E: ty) { struct H { v: E } }\nhold(int)"),
            "(struct H (field v int))"
        );
    }

    #[test]
    fn sigil_item_macro_type_shaped_arg() {
        // Sigil `!(...)` lets `Box<int>` / `[int]` pass as `ty` fragments;
        // `ident` fragment renames the emitted struct.
        assert_eq!(
            cooked_items(
                "macro mk(N: ident, E: ty) { struct N { v: E } }\nmk!(WrapBox, Box<int>)\nmk!(WrapArr, [int])"
            ),
            "(struct WrapBox (field v (Box int))) (struct WrapArr (field v [int]))"
        );
    }

    #[test]
    fn stdlib_surface_forms() {
        // Additional forms used by the standard library:
        // Leading-variadic param `...any` (stdlib spelling) + extern(lua) body.
        assert_eq!(
            item_sexp("extern(lua) fn print(args: ...any)"),
            "(fn print ((args any ...)) () (extern-lua))"
        );
        // `operator(lua)` with a `= \"..\"` body keeps the Lua body kind (the
        // specifier overrides the default), not an empty Rock block.
        assert_eq!(
            item_sexp("@pure @inline operator(lua) ==(a: int, b: int) -> bool = \"return a == b\""),
            "(@pure) (@inline) (fn __op_eq ((a int) (b int)) bool (lua))"
        );
        // Operator symbols only stdlib defines: `~` `<<` `>>`.
        assert_eq!(
            item_sexp("operator(lua) ~(a: int) -> int = \"return bit.bnot(a)\"")
                .split(' ')
                .nth(1)
                .unwrap(),
            "__op_bitnot"
        );
        assert_eq!(
            item_sexp("operator(lua) <<(a: int, b: int) -> int = \"x\"")
                .split(' ')
                .nth(1)
                .unwrap(),
            "__op_shl"
        );
        assert_eq!(
            item_sexp("operator(lua) >>(a: int, b: int) -> int = \"x\"")
                .split(' ')
                .nth(1)
                .unwrap(),
            "__op_shr"
        );
    }

    #[test]
    fn declarations() {
        assert_eq!(
            item_sexp("fn f<T>(x: int, ys: T... , z: int = 3) -> int { x }"),
            "(fn f <T> ((x int) (ys T ...) (z int = 3)) int (block x))"
        );
        assert_eq!(
            item_sexp("value FixVec2 { x: float, y: float }"),
            "(value-struct FixVec2 (field x float) (field y float))"
        );
        assert_eq!(
            item_sexp("struct Foo<T> { a: T, b: int = 0 }"),
            "(struct Foo <T> (field a T) (field b int 0))"
        );
        assert_eq!(
            item_sexp("enum E { A, B(int, float), C { x: int } }"),
            "(enum E (variant A) (variant B (tuple int float)) (variant C (record (field x int))))"
        );
        assert_eq!(item_sexp("newtype Id(int)"), "(newtype Id int)");
        assert_eq!(item_sexp("global N: int = 5"), "(global N int 5)");
        assert_eq!(item_sexp("const PI: float = 3.14"), "(const PI float 3.14)");
        assert_eq!(
            item_sexp("deco init early_init { run() }"),
            "(decorator early_init () (block (call run)))"
        );
        assert_eq!(item_sexp("#if debug"), "(#if \"debug\")");
        // Non-Rock bodies render as a marker.
        assert_eq!(
            item_sexp("extern(ffi, \"native\") fn s(x: int)"),
            "(fn s ((x int)) () (ffi \"native\"))"
        );
    }

    #[test]
    fn annotations_rendered() {
        // Plain + arg'd + named-arg annotations prefix the item.
        assert_eq!(
            item_sexp("@pure @test(output = \"hi\") fn f() { 1 }"),
            "(@pure) (@test output=\"hi\") (fn f () () (block 1))"
        );
        assert_eq!(
            item_sexp("@sim_env(\"K\", \"V\") fn g() { 1 }"),
            "(@sim_env \"K\" \"V\") (fn g () () (block 1))"
        );
        // Block-bodied operators get an implicit `@inline` (rock parity).
        assert_eq!(
            item_sexp("@pure operator +(a: int, b: int) -> int { a + b }"),
            "(@pure) (@inline) (fn __op_add ((a int) (b int)) int (block (+ a b)))"
        );
    }

    #[test]
    fn operator_def() {
        // `operator +` folds to `fn __op_add`; a leading annotation is skipped.
        assert_eq!(
            item0("@pure operator +(a: int, b: int) -> int { a + b }"),
            ("__op_add".to_string(), "(block (+ a b))".to_string())
        );
        assert_eq!(
            item0("operator(lua) ==(a: int, b: int) -> bool { a == b }").0,
            "__op_eq"
        );
        assert_eq!(item0("operator -(a: int) -> int { -a }").0, "__unm");
        assert_eq!(item0("operator !(a: bool) -> bool { !a }").0, "__not");
        assert_eq!(item0("operator ~(a: int) -> int { ~a }").0, "__op_bitnot");
    }

    #[test]
    fn one_parameter_binary_operator_is_rejected() {
        let err = parse_module_clean(
            "struct P { x: int }\noperator +(a: P) -> P { a }",
            Ustr::from("<operator_arity>"),
        )
        .expect_err("a one-parameter binary operator must be rejected");
        assert!(
            err.message.contains("operator `+` takes two parameters"),
            "unexpected diagnostic: {}",
            err.message
        );
    }

    #[test]
    fn annotated_and_macro_items() {
        // Multiple annotations skipped; fn parses normally.
        assert_eq!(
            item0("@pure @test fn g() { 1 }"),
            ("g".to_string(), "(block 1)".to_string())
        );
        // `macro` folds to a function named like the macro; `__SITE__` is an
        // ordinary ident here.
        assert_eq!(
            item0("macro emit(x: expr) { _emit_impl(x, __SITE__) }"),
            (
                "emit".to_string(),
                "(block (call _emit_impl x __SITE__))".to_string()
            )
        );
    }

    #[test]
    fn no_parens_calls() {
        check(
            "start_task \"batch\"",
            "(block (call start_task \"batch\"))",
        );
        check("place_player 20 20", "(block (call place_player 20 20))");
        // Operators (bp >= 15) fold into the argument: `foo(a + b)`, matching
        // rock's `arg_min_bp = 15`.
        check("foo a + b", "(block (call foo (+ a b)))");
    }

    #[test]
    fn multi_statement_block_newline_separated() {
        let src = "fn f() {\n    let x = 1\n    let y = x + 2\n    return y\n}";
        let ast = parse_module(src, Ustr::from("test.rock")).unwrap();
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!()
        };
        assert_eq!(
            ast.block_sexp(f.body.rock_block().unwrap()),
            "(block (let x () 1) (let y () (+ x 2)) (return y))"
        );
    }

    #[test]
    fn inline_lua_body_captures_source() {
        // `inline(lua) fn` must keep the raw Lua source verbatim (codegen emits
        // it). The s-expr still renders a bare `(lua)`, so cook-diff is unaffected.
        let ast = parse_module(
            "inline(lua) fn add(a: int, b: int) -> int = \"\"\"return a + b\"\"\"",
            Ustr::from("test.rock"),
        )
        .expect("parse failed");
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn");
        };
        let FunctionBody::Inline(FnBackend::Lua, src) = &f.body else {
            panic!("expected inline-lua body, got {:?}", f.body);
        };
        assert_eq!(src.as_str(), "return a + b");
    }

    #[test]
    fn inline_js_body_captures_backend_and_source() {
        let ast = parse_module(
            "inline(js) fn add(a: int, b: int) -> int = \"\"\"return a + b\"\"\"",
            Ustr::from("test.rock"),
        )
        .expect("parse failed");
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn");
        };
        let FunctionBody::Inline(FnBackend::Js, src) = &f.body else {
            panic!("expected inline-js body, got {:?}", f.body);
        };
        assert_eq!(src.as_str(), "return a + b");
    }

    #[test]
    fn bare_inline_defaults_to_lua() {
        let ast = parse_module(
            "inline fn add(a: int) -> int = \"\"\"return a\"\"\"",
            Ustr::from("test.rock"),
        )
        .expect("parse failed");
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn");
        };
        assert!(
            matches!(&f.body, FunctionBody::Inline(FnBackend::Lua, _)),
            "got {:?}",
            f.body
        );
    }

    #[test]
    fn extern_js_is_its_own_variant() {
        let ast = parse_module("extern(js) fn now() -> float", Ustr::from("test.rock"))
            .expect("parse failed");
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn");
        };
        assert!(
            matches!(&f.body, FunctionBody::ExternJs),
            "got {:?}",
            f.body
        );
    }

    #[test]
    fn unknown_inline_backend_rejects() {
        let ast = parse_module(
            "inline(wasm) fn add(a: int) -> int = \"\"\"return a\"\"\"",
            Ustr::from("test.rock"),
        )
        .expect("parse recovers");
        assert!(
            ast.recovery_errors
                .iter()
                .any(|e| e.message.contains("unknown inline backend")),
            "unknown inline backend must surface a recovery error, got {:?}",
            ast.recovery_errors
        );
    }

    #[test]
    fn unknown_extern_backend_rejects() {
        let ast = parse_module("extern(wasm) fn now() -> float", Ustr::from("test.rock"))
            .expect("parse recovers");
        assert!(
            ast.recovery_errors
                .iter()
                .any(|e| e.message.contains("unknown extern backend")),
            "unknown extern backend must surface a recovery error, got {:?}",
            ast.recovery_errors
        );
    }

    /// Every block with recorded brace spans must have them sitting on literal
    /// `{` / `}` at its own span's ends; blocks without them are exactly the
    /// parser's synthetic braceless wrappers (lambda `=> expr`, `@`-trailing
    /// lambda, match-arm `=> expr`). Consumers (bone-lower's brace lapper
    /// markers) emit nothing for `braces: None` instead of deriving positions
    /// from `span` -- which planted phantom 1-wide nodes on the first/last
    /// chars of braceless bodies (`bone-query/tests/phantom_block_sentinel.rs`).
    fn check_braces(src: &str, ast: &Ast, expected_braceless: usize) {
        let mut braceless = 0;
        for (_, b) in ast.blocks.iter() {
            match b.braces {
                Some((open, close)) => {
                    let (bs, be) = (b.span.offset_range.0, b.span.offset_range.1);
                    assert_eq!(open.offset_range, (bs, bs + 1), "open brace at span start");
                    assert_eq!(close.offset_range, (be - 1, be), "close brace at span end");
                    assert_eq!(&src[bs..bs + 1], "{", "open brace char");
                    assert_eq!(&src[be - 1..be], "}", "close brace char");
                }
                None => braceless += 1,
            }
        }
        assert_eq!(
            braceless, expected_braceless,
            "synthetic braceless block count"
        );
    }

    #[test]
    fn block_braces_recorded_only_for_real_braces() {
        let src = "enum E { A, B }\n\
                   fn g(h: fn() -> int) -> int { h() }\n\
                   fn f(e: E) -> int {\n\
                   \x20   let a = () => 1\n\
                   \x20   let b = () => { 2 }\n\
                   \x20   let c = g() @ 3\n\
                   \x20   let d = g() @ { 4 }\n\
                   \x20   return match e {\n\
                   \x20       E::A => 5\n\
                   \x20       E::B => { 6 }\n\
                   \x20   }\n\
                   }";
        let ast = parse_module(src, Ustr::from("test.rock")).unwrap();
        // Braceless: lambda `=> 1`, `@ 3`, match arm `=> 5`.
        check_braces(src, &ast, 3);
    }

    #[test]
    fn macro_expanded_blocks_lose_brace_spans() {
        // Span retag stamps the call-site span on expanded template blocks; the
        // template's braces don't exist at the call site, so they must be
        // dropped rather than left pointing at template offsets.
        let src = "macro m() { { 1 } }\nfn f() { m() }";
        let mut modules = vec![parse_module(src, Ustr::from("test.rock")).unwrap()];
        cook(&mut modules);
        let ast = &modules[0];
        let f = ast
            .items
            .iter()
            .find_map(|i| match &i.kind {
                ItemKind::Function(f) if f.name.name.as_str() == "f" => Some(f),
                _ => None,
            })
            .expect("fn f survives cook");
        let body = f.body.rock_block().unwrap();
        // f's own body keeps its real braces...
        assert!(ast.b(&body).braces.is_some(), "fn body braces survive cook");
        // ...but every block spanned to the call site (the retagged expansion,
        // incl. the template's nested braced block) has none.
        let call_span = ast
            .b(&body)
            .statements
            .first()
            .map(|&s| ast.stmts[s].span)
            .unwrap();
        let retagged: Vec<_> = ast
            .blocks
            .iter()
            .filter(|(i, b)| *i != body && b.span == call_span)
            .collect();
        assert!(!retagged.is_empty(), "expected retagged expansion blocks");
        for (_, b) in retagged {
            assert_eq!(b.braces, None, "retagged block must drop brace spans");
        }
    }
}

#[cfg(test)]
mod modules_grammar_tests {
    use super::*;

    fn parse_v1(src: &str) -> Result<Ast, CompilerError> {
        parse_module_clean_with_dialect(src, Ustr::from("test.rock"), Dialect::V1)
    }

    fn body_first_expr(src: &str, dialect: Dialect) -> String {
        let ast =
            parse_module_with_dialect(src, Ustr::from("test.rock"), dialect).expect("parse failed");
        let err_count = ast.recovery_errors.len();
        assert_eq!(
            err_count, 0,
            "clean fixture must not recover: {:?}",
            ast.recovery_errors
        );
        match &ast.items[0].kind {
            ItemKind::Function(f) => {
                let block = f.body.rock_block().unwrap();
                match &ast.blocks[block].statements[..] {
                    [stmt] => match &ast.stmts[*stmt].kind {
                        StatementKind::Let(l) => {
                            let v = l.value.expect("let has a value");

                            ast.expr_sexp(v)
                        }
                        other => panic!("expected let, got {other:?}"),
                    },
                    other => panic!("expected one statement, got {}", other.len()),
                }
            }
            other => panic!("expected fn, got {other:?}"),
        }
    }

    // --- spec 7: contextual ascription -------------------------------------

    #[test]
    fn paren_ascription_parses_in_v1() {
        let sexp = body_first_expr("fn f() { let x = (5 : int) }", Dialect::V1);
        assert_eq!(sexp, "(ascribe 5 int)");
    }

    #[test]
    fn paren_ascription_is_additive_in_legacy() {
        // The form was a parse error in legacy before this branch (probe:
        // "Unsupported binary operator Colon"), so accepting it is additive.
        let sexp = body_first_expr("fn f() { let x = (5 : int) }", Dialect::Legacy);
        assert_eq!(sexp, "(ascribe 5 int)");
    }

    #[test]
    fn legacy_keeps_infix_double_colon_ascription() {
        let sexp = body_first_expr("fn f(x: float) { let y = x :: float }", Dialect::Legacy);
        assert_eq!(sexp, "(:: x float)");
    }

    #[test]
    fn v1_ident_colon_ident_parses_as_a_path_not_ascription() {
        // No name-resolution-based guessing (spec 7): `x :: float` parses as a
        // two-segment path; whether it names anything is the resolver's call.
        let ast = parse_module_with_dialect(
            "fn f(x: float) { let y = x :: float }",
            Ustr::from("test.rock"),
            Dialect::V1,
        )
        .expect("parses");
        assert!(
            ast.recovery_errors.is_empty(),
            "no recovery: {:?}",
            ast.recovery_errors
        );
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn");
        };
        let block = f.body.rock_block().unwrap();
        let stmt = ast.blocks[block].statements[0];
        let StatementKind::Let(l) = &ast.stmts[stmt].kind else {
            panic!("expected let");
        };
        let v = l.value.expect("value");
        assert!(
            matches!(ast.exprs[v].kind, ExprKind::Path { .. }),
            "expected a path, got {:?}",
            ast.exprs[v].kind
        );
    }

    #[test]
    fn v1_rejects_infix_ascription_after_calls() {
        let err = parse_v1("fn f(x: int) { let y = g(x) :: int }")
            .expect_err("v1 must reject infix ascription after a call");
        assert!(
            err.message.contains("(expr : Type)"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn typed_lambda_is_not_an_ascription_in_v1() {
        // `(x : int) => x` — the lambda lookahead wins; `(x : int)` ascription
        // requires the paren group to END the expression.
        let sexp = body_first_expr("fn f() { let g = (x : int) => x }", Dialect::V1);
        assert!(sexp.starts_with("(lambda"), "got: {sexp}");
    }

    #[test]
    fn ascription_of_lambda_value_works_in_v1() {
        let sexp = body_first_expr(
            "fn f() { let cb = ((x => x + 1) : fn(int) -> int) }",
            Dialect::V1,
        );
        assert_eq!(
            sexp,
            "(ascribe (paren (lambda (None) None (block (return (+ x 1))))) (fn (int) int))"
        );
    }

    #[test]
    fn ascription_is_not_a_typed_tuple_in_v1() {
        // `(a : int, b : int)` is not a new typed-tuple literal (spec 7).
        let err = parse_v1("fn f() { let t = (a : int, b : int) }")
            .expect_err("multi-colon paren group must reject");
        assert!(
            err.message.contains("one expression"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn tuple_element_ascription_still_rejects() {
        // Bare `b : int` inside a tuple remains un-added grammar.
        let err = parse_v1("fn f(a: int) { let t = (a, b : int) }")
            .expect_err("bare expr : Type outside parens must reject");
        assert!(
            err.message.contains("parenthesize the ascription"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn nested_tuple_ascription_in_v1() {
        let sexp = body_first_expr(
            "fn f(a: int, b: int) { let t = ((a, b) : (int, int)) }",
            Dialect::V1,
        );
        assert_eq!(sexp, "(ascribe (tuple a b) (tuple int int))");
    }

    #[test]
    fn ascription_groups_tighter_than_operators_in_v1() {
        // `j : float / k : float` without parens is NOT ascription (bare form
        // is un-added); the parenthesized form keeps its existing grouping.
        let sexp = body_first_expr(
            "fn f(j: int, k: int) { let d = (j : float) / (k : float) }",
            Dialect::V1,
        );
        assert_eq!(sexp, "(/ (ascribe j float) (ascribe k float))");
    }
}

#[cfg(test)]
mod modules_path_tests {
    use super::*;

    fn parse_v1(src: &str) -> Result<Ast, CompilerError> {
        parse_module_clean_with_dialect(src, Ustr::from("test.rock"), Dialect::V1)
    }

    fn let_type(src: &str) -> String {
        let ast = parse_v1(src).expect("parse failed");
        assert!(ast.recovery_errors.is_empty(), "{:?}", ast.recovery_errors);
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn")
        };
        let block = f.body.rock_block().unwrap();
        let StatementKind::Let(l) = &ast.stmts[ast.blocks[block].statements[0]].kind else {
            panic!("expected let");
        };
        let ty = l.ty.expect("let has a type annotation");
        ast.type_sexp(ty)
    }

    fn let_value(src: &str) -> String {
        let ast = parse_v1(src).expect("parse failed");
        assert!(ast.recovery_errors.is_empty(), "{:?}", ast.recovery_errors);
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn")
        };
        let block = f.body.rock_block().unwrap();
        let StatementKind::Let(l) = &ast.stmts[ast.blocks[block].statements[0]].kind else {
            panic!("expected let");
        };
        ast.expr_sexp(l.value.expect("value"))
    }

    #[test]
    fn qualified_type_path_parses_in_v1() {
        assert_eq!(
            let_type("fn f() { let x: sample::combat::Actor = x }"),
            "sample::combat::Actor"
        );
    }

    #[test]
    fn generic_type_path_parses_in_v1() {
        assert_eq!(
            let_type("fn f() { let x: native::collections::Vec<int> = x }"),
            "(native::collections::Vec int)"
        );
    }

    #[test]
    fn root_anchored_and_self_super_type_paths_parse_in_v1() {
        assert_eq!(
            let_type("fn f() { let x: ::app::Actor = x }"),
            "::app::Actor"
        );
        assert_eq!(let_type("fn f() { let x: self::Actor = x }"), "self::Actor");
        assert_eq!(
            let_type("fn f() { let x: super::shared::T = x }"),
            "super::shared::T"
        );
        assert_eq!(
            let_type("fn f() { let x: super::super::T = x }"),
            "super::super::T"
        );
    }

    #[test]
    fn value_path_parses_in_v1() {
        assert_eq!(
            let_value("fn f() { let h = sample::combat::heal }"),
            "sample::combat::heal"
        );
    }

    #[test]
    fn lowercase_member_is_a_path_in_v1_no_case_guess() {
        // Legacy requires the member to be uppercase for a qualified ident;
        // v1 has no case rule (the resolver decides).
        assert_eq!(
            let_value("fn f() { let v = mod_u::helper }"),
            "mod_u::helper"
        );
    }

    #[test]
    fn qualified_call_parses_in_v1() {
        let ast = parse_v1("fn f() { let v = sample::combat::heal(actor, 5)\nprint(v) }")
            .expect("parse failed");
        assert!(ast.recovery_errors.is_empty(), "{:?}", ast.recovery_errors);
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn")
        };
        let block = f.body.rock_block().unwrap();
        let StatementKind::Let(l) = &ast.stmts[ast.blocks[block].statements[0]].kind else {
            panic!("expected let");
        };
        let v = l.value.expect("value");
        let ExprKind::FunctionCall { func, .. } = &ast.exprs[v].kind else {
            panic!("expected call, got {:?}", ast.exprs[v].kind);
        };
        assert!(matches!(ast.exprs[*func].kind, ExprKind::Path { .. }));
    }

    #[test]
    fn qualified_generic_call_parses_in_v1() {
        let ast = parse_v1("fn f() { let v = p::make<int>(3)\nprint(v) }").expect("parse failed");
        assert!(ast.recovery_errors.is_empty(), "{:?}", ast.recovery_errors);
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn")
        };
        let block = f.body.rock_block().unwrap();
        let StatementKind::Let(l) = &ast.stmts[ast.blocks[block].statements[0]].kind else {
            panic!("expected let");
        };
        let v = l.value.expect("value");
        let ExprKind::FunctionCall { generic_args, .. } = &ast.exprs[v].kind else {
            panic!("expected call, got {:?}", ast.exprs[v].kind);
        };
        assert_eq!(generic_args.len(), 1, "explicit type argument survived");
    }

    #[test]
    fn qualified_enum_variant_is_an_ordinary_path_in_v1() {
        // In v1 `Enum::Variant` is a plain path: the case of the second
        // segment must not decide anything at parse time.
        assert_eq!(let_value("fn f() { let c = Color::Red }"), "Color::Red");
        assert_eq!(
            let_value("fn f() { let c = colors::Color::Red }"),
            "colors::Color::Red"
        );
    }

    #[test]
    fn qualified_struct_literal_parses_in_v1() {
        let ast =
            parse_v1("fn f() { let a = lib::Actor { hp: 100 }\nprint(a) }").expect("parse failed");
        assert!(ast.recovery_errors.is_empty(), "{:?}", ast.recovery_errors);
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn")
        };
        let block = f.body.rock_block().unwrap();
        let StatementKind::Let(l) = &ast.stmts[ast.blocks[block].statements[0]].kind else {
            panic!("expected let");
        };
        let v = l.value.expect("value");
        let ExprKind::PathStructLiteral { path, fields, .. } = &ast.exprs[v].kind else {
            panic!("expected path struct literal, got {:?}", ast.exprs[v].kind);
        };
        assert_eq!(path.segments.len(), 2);
        assert_eq!(fields.len(), 1);
    }

    #[test]
    fn qualified_pattern_parses_in_v1() {
        let ast = parse_v1(
            "fn f(msg: Message) { let x = match msg { msg_kind::Ping => 1, other => 0 }\nprint(x) }",
        )
        .expect("parse failed");
        assert!(ast.recovery_errors.is_empty(), "{:?}", ast.recovery_errors);
        // The arm pattern is a PathVariant.
        let found = ast.blocks.iter().any(|_| true);
        assert!(found);
        // Depth-check via sexp: match arms render patterns.
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn")
        };
        let block = f.body.rock_block().unwrap();
        let sexp = ast.block_sexp(block);
        assert!(
            sexp.contains("msg_kind::Ping"),
            "pattern should render as a path: {sexp}"
        );
    }

    #[test]
    fn v1_still_parses_unqualified_single_segment_idents() {
        // A bare `Foo` in v1 stays a plain ident (one-segment resolution is
        // the common case); it must not become a Path node.
        assert_eq!(let_value("fn f() { let v = helper }"), "helper");
    }

    #[test]
    fn legacy_qualified_ident_case_rule_unchanged() {
        // Legacy: `Color::Red` is a QualifiedIdent; `mod_u::helper` is NOT
        // (lowercase member falls through to ascription/ident handling).
        let ast = parse_module(
            "fn f() { let c = Color::Red\nprint(c) }",
            Ustr::from("test.rock"),
        )
        .expect("parse");
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn")
        };
        let block = f.body.rock_block().unwrap();
        let StatementKind::Let(l) = &ast.stmts[ast.blocks[block].statements[0]].kind else {
            panic!("expected let");
        };
        assert!(matches!(
            ast.exprs[l.value.expect("value")].kind,
            ExprKind::QualifiedIdent { .. }
        ));
    }
}

#[cfg(test)]
mod modules_item_tests {
    use super::*;

    fn parse_v1(src: &str) -> Result<Ast, CompilerError> {
        parse_module_clean_with_dialect(src, Ustr::from("test.rock"), Dialect::V1)
    }

    #[test]
    fn qualified_d_form_receiver_parses_in_v1() {
        let ast = parse_v1("fn lib::Actor.heal(a, amount: int) { a.hp = a.hp + amount }")
            .expect("parse failed");
        assert!(ast.recovery_errors.is_empty(), "{:?}", ast.recovery_errors);
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn")
        };
        assert_eq!(f.name.name, "heal");
        assert!(f.receiver_surface.is_some(), "receiver surface recorded");
    }
}

#[cfg(test)]
mod modules_items_tests {
    use super::*;

    fn parse_v1(src: &str) -> Result<Ast, CompilerError> {
        parse_module_clean_with_dialect(src, Ustr::from("test.rock"), Dialect::V1)
    }

    #[test]
    fn file_header_parses_in_v1() {
        let ast = parse_v1("mod combat::casting;\n\nfn f() {}").expect("parse");
        let h = ast.module_header.as_ref().expect("header recorded");
        let names: Vec<_> = h.path.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["combat", "casting"]);
        assert!(h.vis.is_none());
    }

    #[test]
    fn pub_file_header_and_explicit_root_parse_in_v1() {
        let ast = parse_v1("pub mod combat;\n\nfn f() {}").expect("parse");
        let h = ast.module_header.as_ref().expect("header");
        assert_eq!(h.path.len(), 1);
        assert_eq!(h.vis, Some(Visibility::Pub));

        let ast = parse_v1("mod self;\n\nfn f() {}").expect("parse");
        let h = ast.module_header.as_ref().expect("header");
        assert!(h.path.is_empty(), "mod self; is the explicit root");
    }

    #[test]
    fn misplaced_header_rejects_in_v1() {
        let err = parse_v1("fn f() {}\nmod combat;").expect_err("header must lead the file");
        assert!(err.message.contains("first item"), "got: {}", err.message);
    }

    /// Only a `;`-terminated `mod` is a header; an inline `mod name { .. }`
    /// block may follow any item, including another block.
    #[test]
    fn inline_module_blocks_follow_items_in_v1() {
        for src in [
            "mod a {}\nmod b {}",
            "fn f() {}\npub mod b { fn g() {} }",
            "mod a { mod b {} mod c {} }",
        ] {
            let ast = parse_v1(src).unwrap_or_else(|e| panic!("{src:?}: {}", e.message));
            assert!(ast.module_header.is_none(), "{src:?}");
            assert!(
                ast.items
                    .iter()
                    .all(|i| matches!(i.kind, ItemKind::Module(_))
                        || matches!(i.kind, ItemKind::Function(_))),
                "{src:?}"
            );
        }
        let ast = parse_v1("mod a {}\nmod b {}").expect("parse");
        assert_eq!(ast.items.len(), 2);
    }

    #[test]
    fn misplaced_header_after_an_inline_block_rejects_in_v1() {
        for src in [
            "mod a {}\nmod combat;",
            "fn f() {}\nmod combat::casting;",
            "fn f() {}\npub mod combat;",
            "mod a { mod b; }",
            "mod a { mod b::c; }",
        ] {
            let err = parse_v1(src).expect_err(src);
            assert!(
                err.message.contains("first item"),
                "{src:?} got: {}",
                err.message
            );
        }
    }

    #[test]
    fn a_second_file_header_rejects_in_v1() {
        for src in [
            "mod a;\nmod b;\n\nfn f() {}",
            "mod a;\n// between\npub mod a;\n",
        ] {
            let err = parse_v1(src).expect_err("a file has one header");
            assert!(
                err.message.contains("a file has one `mod path;` header"),
                "got: {}",
                err.message
            );
        }
    }

    /// Visibility leads the item, annotations included (`parse_item` reads the
    /// modifiers before the annotations), so an annotated item and a custom
    /// decorator can be exported.
    #[test]
    fn visibility_leads_an_annotated_item_and_a_decorator() {
        let spelled = |vis: &Option<Visibility>| match vis {
            Some(Visibility::PubIn(path)) => {
                let path: Vec<&str> = path.iter().map(|i| i.name.as_str()).collect();
                format!("pub(in {})", path.join("::"))
            }
            other => format!("{other:?}"),
        };
        for (src, want) in [
            ("pub @inline fn f() -> int { 1 }", "Some(Pub)"),
            (
                "pub(package) @pure @inline\nfn f() -> int { 1 }",
                "Some(PubPackage)",
            ),
            (
                "private @nomangle inline(lua) fn f() -> int = \"\"\"return 1\"\"\"",
                "Some(Private)",
            ),
            ("pub deco traced(f: fn()) { }", "Some(Pub)"),
            // `in` is a keyword token, so this form never parsed at item level.
            (
                "pub(in combat::casting) fn f() -> int { 1 }",
                "pub(in combat::casting)",
            ),
        ] {
            let ast = parse_v1(src).expect(src);
            assert!(
                ast.recovery_errors.is_empty(),
                "{src}: {:?}",
                ast.recovery_errors
            );
            assert_eq!(spelled(&ast.items[0].vis), want, "{src}");
        }
        let ast = parse_v1("pub @inline fn f() -> int { 1 }").expect("parse");
        assert_eq!(ast.items[0].annotations.len(), 1);
    }

    #[test]
    fn inline_mod_block_parses_in_v1() {
        let ast = parse_v1("pub mod combat {\n    pub fn heal() {}\n}\n").expect("parse");
        assert!(ast.recovery_errors.is_empty(), "{:?}", ast.recovery_errors);
        let ItemKind::Module(m) = &ast.items[0].kind else {
            panic!("expected mod item")
        };
        assert_eq!(m.name.name, "combat");
        assert_eq!(m.vis, Some(Visibility::Pub));
        assert_eq!(m.items.len(), 1);
    }

    #[test]
    fn use_forms_parse_in_v1_and_legacy() {
        let cases = [
            "use a::b::c;",
            "use a::b::*;",
            "use a::b as c;",
            "use a::b::{Actor, heal as h};",
            "use a::b::{self, Actor};",
            "use geo::operator(+);",
            "use geo::operator(+) as add_pt;",
            "use geo::operator(!);",
            "use geo::{Pt, operator(*) as scale};",
            "use ::rooted::a;",
            "pub use a::b::*;",
            "pub(package) use a::b::step;",
            "private use a::b::x;",
        ];
        // Bare `use` parses additively in legacy too (contextual, plan M1);
        // visibility-prefixed forms are v1 grammar.
        let legacy_cases = [
            "use a::b::c;",
            "use a::b::*;",
            "use a::b as c;",
            "use a::b::{Actor, heal as h};",
            "use a::b::{self, Actor};",
            "use geo::operator(+);",
            "use geo::operator(+) as add_pt;",
            "use geo::operator(!);",
            "use geo::{Pt, operator(*) as scale};",
            "use ::rooted::a;",
        ];
        for src in cases {
            let ast = parse_v1(src).expect(src);
            assert!(
                ast.recovery_errors.is_empty(),
                "{src}: {:?}",
                ast.recovery_errors
            );
            assert!(matches!(ast.items[0].kind, ItemKind::Use(_)), "{src}");
            if legacy_cases.contains(&src) {
                let ast = parse_module(src, Ustr::from("legacy.rock")).expect(src);
                assert!(
                    ast.recovery_errors.is_empty(),
                    "{src}: {:?}",
                    ast.recovery_errors
                );
            }
        }
    }

    #[test]
    fn use_as_underscore_rejects() {
        let err = parse_v1("use a::b as _;").expect_err("as _ must reject");
        assert!(err.message.contains("as _"), "got: {}", err.message);
    }

    #[test]
    fn an_operator_selector_takes_an_alias() {
        let ast = parse_v1("use geo::operator(+) as add_pt;").expect("parse");
        let ItemKind::Use(u) = &ast.items[0].kind else {
            panic!("expected use")
        };
        let UseTree::Path {
            kind: UseSuffix::Operator { op, alias },
            segments,
            ..
        } = &u.tree
        else {
            panic!("expected an operator selector, got {:?}", u.tree);
        };
        assert_eq!(
            segments.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["geo"]
        );
        assert_eq!(op.name, "__op_add");
        assert_eq!(alias.as_ref().map(|a| a.name.as_str()), Some("add_pt"));

        let ast = parse_v1("use geo::operator(!);").expect("parse unary selector");
        let ItemKind::Use(u) = &ast.items[0].kind else {
            panic!("expected use")
        };
        let UseTree::Path {
            kind: UseSuffix::Operator { op, alias },
            ..
        } = &u.tree
        else {
            panic!("expected an operator selector, got {:?}", u.tree);
        };
        assert_eq!(op.name, "__not");
        assert!(alias.is_none());

        let err = parse_v1("use geo::operator(+) as _;").expect_err("as _ must reject");
        assert!(err.message.contains("as _"), "got: {}", err.message);
    }

    #[test]
    fn nested_use_trees_parse() {
        let ast = parse_v1("use a::b::{c::{d, e}, f::g as h};").expect("parse");
        let ItemKind::Use(u) = &ast.items[0].kind else {
            panic!("expected use")
        };
        let UseTree::Path {
            kind: UseSuffix::Nested(trees),
            ..
        } = &u.tree
        else {
            panic!("expected nested");
        };
        assert_eq!(trees.len(), 2);
    }

    #[test]
    fn private_set_field_parses_in_v1() {
        let ast = parse_v1(
            "pub struct Actor {\n    name: string,\n    private(set) health: float = 100,\n    pub(package) private(set) counter: int,\n}",
        )
        .expect("parse");
        let ItemKind::Struct(s) = &ast.items[0].kind else {
            panic!("expected struct")
        };
        assert!(s.fields[0].vis.is_none());
        assert!(s.fields[1].set_private);
        assert!(s.fields[2].set_private);
        assert_eq!(s.fields[2].vis, Some(Visibility::PubPackage));
    }

    #[test]
    fn contextual_words_stay_usable_as_names() {
        // `private: bool` is a field named `private`; `mod` and `use` do not
        // reserve anything (contextual words only).
        let ast = parse_v1(
            "struct S {\n    private: bool,\n    pub: int,\n}\nfn f(p: S) { let x = p\nprint(x) }",
        )
        .expect("parse");
        assert!(ast.recovery_errors.is_empty(), "{:?}", ast.recovery_errors);
    }

    #[test]
    fn vis_on_items_parses_in_v1() {
        let ast = parse_v1("pub fn f() {}\nprivate struct S {}\npub(package) global G: int = 0")
            .expect("parse");
        assert_eq!(ast.items[0].vis, Some(Visibility::Pub));
        assert_eq!(ast.items[1].vis, Some(Visibility::Private));
        assert_eq!(ast.items[2].vis, Some(Visibility::PubPackage));
    }

    #[test]
    fn private_set_on_an_item_rejects() {
        let err = parse_v1("private(set) fn f() {}").expect_err("private(set) is field-only");
        assert!(err.message.contains("fields"), "got: {}", err.message);
    }

    #[test]
    fn block_level_use_parses() {
        let ast = parse_v1("fn f() {\n    use a::b::c\n    let x = 1\n}").expect("parse");
        assert!(ast.recovery_errors.is_empty(), "{:?}", ast.recovery_errors);
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected fn")
        };
        let block = f.body.rock_block().unwrap();
        assert!(matches!(
            ast.stmts[ast.blocks[block].statements[0]].kind,
            StatementKind::Use(_)
        ));
    }
}

#[cfg(test)]
mod vis_probe_tests {
    use super::*;
    #[test]
    fn probe_vis_debug() {
        let src = "struct S {\n    private(set) health: float = 100,\n}";
        let ast = parse_module(src, Ustr::from("p.rock")).expect("parse");
        // legacy: vis not parsed; private(set) must FAIL parse or be a field named private
        let _ = ast;
    }
}
