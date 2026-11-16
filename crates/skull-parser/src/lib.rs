//! Arena-allocated surface AST and parser, built on `rock-tokenizer`
//! (lexer) and `rock-core` (spans, diagnostics).

pub mod ast;
pub mod desugar;
pub(crate) mod graft;
pub mod macro_expand;
pub mod parser;
pub mod stdlib;

pub use ast::*;
pub(crate) use parser::Parser;

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
    check_macro_name_uniqueness, collect_registry, is_hygiene_name, macro_reach,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Macro-expand across `modules`, then desugar each one.
    fn cook(modules: &mut [Ast]) {
        macro_expand::expand_macros(modules);
        for ast in modules.iter_mut() {
            desugar::desugar(ast);
        }
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
        // offset/line/col) must preserve the item and body spans so the
        // incremental driver can swap it in. The middle
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
        let (s, e) = item.span.offset_range();
        let sub = &src[s..e];

        let mini = parse_one_item(
            sub,
            file,
            s,
            item.span.line_range().0,
            item.span.col_range().0,
            Dialect::Legacy,
        )
        .expect("parse_one_item");
        assert_eq!(mini.items.len(), 1, "exactly one item parsed");
        let m = &mini.items[0];

        assert_eq!(
            m.span.offset_range(),
            item.span.offset_range(),
            "item offset span"
        );
        assert_eq!(
            m.span.line_range(),
            item.span.line_range(),
            "item line range"
        );

        let body_block = |it: &Item| match &it.kind {
            ItemKind::Function(f) => f.body.rock_block().unwrap(),
            other => panic!("expected fn, got {other:?}"),
        };
        let mb = body_block(m);
        let fb = body_block(item);
        // A nested node's absolute span: wrong base offset would shift it.
        assert_eq!(
            mini.b(&mb).span.offset_range(),
            full.b(&fb).span.offset_range(),
            "body block span"
        );
        assert_eq!(
            mini.blocks[mb].statements.len(),
            full.blocks[fb].statements.len()
        );
    }

    #[test]
    fn parse_one_item_annotated() {
        // item.span covers leading annotations -- the slice must start at `@`.
        let src = "fn pre() {}\n@test\nfn tagged() -> int { 7 }\n";
        let file = Ustr::from("test.rock");
        let full = parse_module(src, file).expect("full parse");
        let item = &full.items[1];
        let (s, e) = item.span.offset_range();
        let mini = parse_one_item(
            &src[s..e],
            file,
            s,
            item.span.line_range().0,
            item.span.col_range().0,
            Dialect::Legacy,
        )
        .expect("parse_one_item");
        assert_eq!(
            mini.items[0].span.offset_range(),
            item.span.offset_range(),
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
            a.span.offset_range().0 < a.span.offset_range().1,
            "anno span non-empty"
        );
        assert_ne!(
            a.span.offset_range(),
            f.params[0].name.span.offset_range(),
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
        // real arena nodes.
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
                    let (s, e) = x.span.offset_range();
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
        let (s, e) = item.span.offset_range();
        let mini = parse_one_item(
            &src[s..e],
            file,
            s,
            item.span.line_range().0,
            item.span.col_range().0,
            Dialect::Legacy,
        )
        .expect("parse_one_item");
        for (name, text) in ident_spans(&mini, src) {
            assert_eq!(text, name, "reparsed ident span does not cover its source");
        }
    }

    #[test]
    fn keyword_spans_and_dropped_fields_are_captured() {
        // Keyword spans and explicit types must survive parse.
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
        // Leading-dot sibling references retain their own expression kind.
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
    fn inline_lua_body_captures_source() {
        // `inline(lua) fn` must keep the raw Lua source verbatim (codegen emits
        // it).
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
                    let (bs, be) = (b.span.offset_range().0, b.span.offset_range().1);
                    assert_eq!(
                        open.offset_range(),
                        (bs, bs + 1),
                        "open brace at span start"
                    );
                    assert_eq!(
                        close.offset_range(),
                        (be - 1, be),
                        "close brace at span end"
                    );
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

    // --- contextual ascription ----------------------------------------------

    #[test]
    fn v1_ident_colon_ident_parses_as_a_path_not_ascription() {
        // No name-resolution-based guessing: `x :: float` parses as a
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
    fn ascription_is_not_a_typed_tuple_in_v1() {
        // `(a : int, b : int)` is not a new typed-tuple literal.
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
}

#[cfg(test)]
mod modules_path_tests {
    use super::*;

    fn parse_v1(src: &str) -> Result<Ast, CompilerError> {
        parse_module_clean_with_dialect(src, Ustr::from("test.rock"), Dialect::V1)
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
        // Bare `use` parses additively in legacy too (contextual);
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
mod surface_ast_tests {
    use super::*;

    fn parse(src: &str, dialect: Dialect) -> Ast {
        parse_module_clean_with_dialect(src, Ustr::from("test.rock"), dialect).unwrap()
    }

    fn body(ast: &Ast) -> &BlockData {
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected function")
        };
        &ast.blocks[f.body.rock_block().unwrap()]
    }

    fn value(ast: &Ast) -> Idx<ExprData> {
        let StatementKind::Let(l) = &ast.stmts[body(ast).statements[0]].kind else {
            panic!("expected let")
        };
        l.value.unwrap()
    }

    fn expression(src: &str, dialect: Dialect) -> (Ast, Idx<ExprData>) {
        let ast = parse(&format!("fn f() {{ let result = {src} }}"), dialect);
        let id = value(&ast);
        (ast, id)
    }

    fn binary(ast: &Ast, id: Idx<ExprData>, expected: TokenKind) -> (Idx<ExprData>, Idx<ExprData>) {
        let ExprKind::BinOp {
            left, op, right, ..
        } = &ast.exprs[id].kind
        else {
            panic!("expected binary expression: {:?}", ast.exprs[id].kind)
        };
        assert_eq!(*op, expected);
        (*left, *right)
    }

    #[test]
    fn operators_preserve_precedence_and_left_associativity() {
        let (ast, id) = expression("1 + 2 * 3 - 4", Dialect::Legacy);
        let (left, right) = binary(&ast, id, TokenKind::Minus);
        assert!(matches!(ast.exprs[right].kind, ExprKind::Int(4, _)));
        let (_, product) = binary(&ast, left, TokenKind::Plus);
        binary(&ast, product, TokenKind::Mul);
        let (ast, id) = expression("a < b && c > d", Dialect::Legacy);
        let (left, right) = binary(&ast, id, TokenKind::And);
        binary(&ast, left, TokenKind::LessThan);
        binary(&ast, right, TokenKind::GreaterThan);
    }

    #[test]
    fn leading_operator_continues_across_newline() {
        let (ast, id) = expression("10\n - 1\n - 2", Dialect::Legacy);
        let (left, _) = binary(&ast, id, TokenKind::Minus);
        binary(&ast, left, TokenKind::Minus);
    }

    #[test]
    fn guarded_match_keeps_all_arms_and_struct_bodies() {
        for guard in ["x", "(x)", "x && x"] {
            let (ast, id) = expression(
                &format!("match e {{ E::A if {guard} => S {{ x: 1 }} E::B => S {{ x: 2 }} }}"),
                Dialect::Legacy,
            );
            let ExprKind::Match { arms, .. } = &ast.exprs[id].kind else {
                panic!("expected match")
            };
            assert_eq!(arms.len(), 2);
            assert!(arms[0].guard.is_some());
            assert!(arms[1].guard.is_none());
            for arm in arms {
                let StatementKind::Expression(e) =
                    ast.stmts[ast.blocks[arm.body].statements[0]].kind
                else {
                    panic!("expected expression")
                };
                assert!(matches!(ast.exprs[e].kind, ExprKind::StructLiteral { .. }));
            }
        }
    }

    #[test]
    fn recovery_preserves_fields_and_following_bindings() {
        let ast = parse_module(
            "fn f() { let p = P { a: 1 b: 2 }\nlet broken =\nlet kept = 3 }",
            Ustr::from("test.rock"),
        )
        .unwrap();
        assert!(!ast.recovery_errors.is_empty());
        let ExprKind::StructLiteral { fields, .. } = &ast.exprs[value(&ast)].kind else {
            panic!("expected struct")
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].0[0].name, "a");
        assert_eq!(fields[1].0[0].name, "b");
        assert_eq!(body(&ast).statements.len(), 2);
        let StatementKind::Let(l) = &ast.stmts[body(&ast).statements[1]].kind else {
            panic!("expected let")
        };
        assert!(matches!(&l.binding, Binding::Ident(i) if i.name == "kept"));
    }

    #[test]
    fn interpolation_nested_colons_and_strings_stay_in_the_expression() {
        for source in [
            r#""{add2(2, b: 5)}""#,
            r#""{P { v: 1 }}""#,
            r#""{g(h(a: 1), b: 2)}""#,
            r#""{join("a[b]c".split("["), "|")}""#,
            r#""{greet("a:b")}""#,
        ] {
            let (ast, id) = expression(source, Dialect::Legacy);
            let ExprKind::InterpolatedString(parts) = &ast.exprs[id].kind else {
                panic!("expected interpolation")
            };
            let [InterpolatedPart::Expr(e, None)] = parts.as_slice() else {
                panic!("colon became a format spec: {parts:?}")
            };
            assert!(matches!(
                ast.exprs[*e].kind,
                ExprKind::FunctionCall { .. } | ExprKind::StructLiteral { .. }
            ));
        }
        let (ast, id) = expression(r#""{xs[0]:3}""#, Dialect::Legacy);
        let ExprKind::InterpolatedString(parts) = &ast.exprs[id].kind else {
            panic!("expected interpolation")
        };
        let InterpolatedPart::Expr(e, Some(spec)) = &parts[0] else {
            panic!("expected format spec")
        };
        assert!(matches!(ast.exprs[*e].kind, ExprKind::Index(..)));
        assert_eq!(spec.width, Some(3));
    }

    #[test]
    fn ascription_and_lambda_disambiguation() {
        for dialect in [Dialect::Legacy, Dialect::V1] {
            let (ast, id) = expression("(5 : int)", dialect);
            let ExprKind::ParenAscription(e, ty) = ast.exprs[id].kind else {
                panic!("expected ascription")
            };
            assert!(matches!(ast.exprs[e].kind, ExprKind::Int(5, _)));
            assert!(matches!(ast.types[ty].kind, TypeExprKind::Int));
        }
        let (ast, id) = expression("x :: float", Dialect::Legacy);
        assert!(matches!(ast.exprs[id].kind, ExprKind::TypeAscription(..)));
        let (ast, id) = expression("(x : int) => x", Dialect::V1);
        assert!(matches!(ast.exprs[id].kind, ExprKind::Lambda { .. }));
        let (ast, id) = expression("(j : float) / (k : float)", Dialect::V1);
        let (left, right) = binary(&ast, id, TokenKind::Div);
        assert!(matches!(
            ast.exprs[left].kind,
            ExprKind::ParenAscription(..)
        ));
        assert!(matches!(
            ast.exprs[right].kind,
            ExprKind::ParenAscription(..)
        ));
    }

    #[test]
    fn paths_keep_segments_root_and_generic_arguments() {
        for (source, root, names) in [
            ("::app::Actor", true, vec!["app", "Actor"]),
            ("self::Actor", false, vec!["self", "Actor"]),
            ("super::super::T", false, vec!["super", "super", "T"]),
            ("mod_u::helper", false, vec!["mod_u", "helper"]),
        ] {
            let (ast, id) = expression(source, Dialect::V1);
            let ExprKind::Path { path, .. } = &ast.exprs[id].kind else {
                panic!("expected path")
            };
            assert_eq!(path.from_root, root);
            assert_eq!(
                path.segments
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>(),
                names
            );
        }
        let ast = parse(
            "fn f() { let x: native::collections::Vec<int> = x }",
            Dialect::V1,
        );
        let StatementKind::Let(l) = &ast.stmts[body(&ast).statements[0]].kind else {
            panic!("expected let")
        };
        let TypeExprKind::AppPath { ctor, args } = &ast.types[l.ty.unwrap()].kind else {
            panic!("expected generic path")
        };
        assert_eq!(ctor.segments.len(), 3);
        assert_eq!(args.len(), 1);
        assert!(matches!(ast.types[args[0]].kind, TypeExprKind::Int));
        let (ast, id) = expression("match msg { msg_kind::Ping => 1, other => 0 }", Dialect::V1);
        let ExprKind::Match { arms, .. } = &ast.exprs[id].kind else {
            panic!("expected match")
        };
        assert!(
            matches!(&arms[0].pattern, Pattern::PathVariant { path, .. } if path.segments[0].name == "msg_kind" && path.segments[1].name == "Ping")
        );
    }

    #[test]
    fn single_and_multiple_if_let_bindings_use_chains() {
        for (source, count) in [
            ("if let x = foo() { x } else { 0 }", 1),
            ("if let x = foo() && let y = bar() { y }", 2),
        ] {
            let (ast, id) = expression(source, Dialect::Legacy);
            let ExprKind::IfLetChain { clauses, .. } = &ast.exprs[id].kind else {
                panic!("expected binding chain")
            };
            assert_eq!(clauses.len(), count);
            assert!(
                clauses
                    .iter()
                    .all(|c| matches!(c, IfLetClause::Binding { .. }))
            );
        }
    }

    #[test]
    fn item_macros_splice_and_substitute_type_fragments() {
        let mut ast = parse(
            "macro mk(N: ident, E: ty) { struct N { v: E } }\nmk!(WrapBox, Box<int>)\nmk!(WrapArr, [int])",
            Dialect::Legacy,
        );
        macro_expand::expand_macros(std::slice::from_mut(&mut ast));
        desugar::desugar(&mut ast);
        let structs: Vec<_> = ast
            .items
            .iter()
            .filter_map(|i| match &i.kind {
                ItemKind::Struct(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(structs.len(), 2);
        assert_eq!(structs[0].name.name, "WrapBox");
        assert_eq!(structs[1].name.name, "WrapArr");
        assert!(matches!(
            ast.types[structs[0].fields[0].ty].kind,
            TypeExprKind::App { .. }
        ));
        assert!(matches!(
            ast.types[structs[1].fields[0].ty].kind,
            TypeExprKind::Array(_)
        ));
    }
}

#[cfg(test)]
mod statement_shape_tests {
    use super::*;

    #[test]
    fn local_declarations_preserve_storage_types_and_nested_functions() {
        let ast = parse_module_clean("fn f() { static count = 0\nstatic scratch: [int] = []\nfn inner(x: int) -> int { x }\nlet m: Map<int, Vec<int>> = a }", Ustr::from("test.rock")).unwrap();
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected function")
        };
        let stmts = &ast.blocks[f.body.rock_block().unwrap()].statements;
        assert_eq!(stmts.len(), 4);
        let StatementKind::Static(s) = &ast.stmts[stmts[0]].kind else {
            panic!("expected static")
        };
        assert_eq!(s.name.name, "count");
        assert!(s.ty.is_none());
        assert!(matches!(ast.exprs[s.value].kind, ExprKind::Int(0, _)));
        let StatementKind::Static(s) = &ast.stmts[stmts[1]].kind else {
            panic!("expected static")
        };
        assert!(matches!(
            ast.types[s.ty.unwrap()].kind,
            TypeExprKind::Array(_)
        ));
        let StatementKind::NestedFn(n) = &ast.stmts[stmts[2]].kind else {
            panic!("expected nested function")
        };
        assert_eq!(n.name.name, "inner");
        assert_eq!(n.params.len(), 1);
        assert!(matches!(
            ast.types[n.return_ty.unwrap()].kind,
            TypeExprKind::Int
        ));
        let StatementKind::Let(l) = &ast.stmts[stmts[3]].kind else {
            panic!("expected let")
        };
        let TypeExprKind::App { args, .. } = &ast.types[l.ty.unwrap()].kind else {
            panic!("expected generic type")
        };
        assert_eq!(args.len(), 2);
        assert!(matches!(ast.types[args[1]].kind, TypeExprKind::App { .. }));
    }

    #[test]
    fn call_spreads_and_struct_bases_keep_source_order() {
        let ast = parse_module_clean("fn f() { draw(img, ..props, color: c)\nlet p = P { ..a, ..b, c: 1 }\nworkgroup_array<uint>(64) }", Ustr::from("test.rock")).unwrap();
        let ItemKind::Function(f) = &ast.items[0].kind else {
            panic!("expected function")
        };
        let stmts = &ast.blocks[f.body.rock_block().unwrap()].statements;
        let StatementKind::Expression(call) = ast.stmts[stmts[0]].kind else {
            panic!("expected call")
        };
        let ExprKind::FunctionCall { args, .. } = &ast.exprs[call].kind else {
            panic!("expected call")
        };
        assert_eq!(args.len(), 3);
        assert!(!args[0].is_spread);
        assert!(args[1].is_spread);
        assert_eq!(args[2].name.as_ref().unwrap().name, "color");
        let StatementKind::Let(l) = &ast.stmts[stmts[1]].kind else {
            panic!("expected let")
        };
        let ExprKind::StructLiteral { bases, fields, .. } = &ast.exprs[l.value.unwrap()].kind
        else {
            panic!("expected struct")
        };
        assert_eq!(bases.len(), 2);
        assert!(matches!(&ast.exprs[bases[0]].kind, ExprKind::Ident(i) if i.name == "a"));
        assert!(matches!(&ast.exprs[bases[1]].kind, ExprKind::Ident(i) if i.name == "b"));
        assert_eq!(fields.len(), 1);
        let StatementKind::Expression(call) = ast.stmts[stmts[2]].kind else {
            panic!("expected call")
        };
        let ExprKind::FunctionCall { generic_args, .. } = &ast.exprs[call].kind else {
            panic!("expected call")
        };
        assert_eq!(generic_args.len(), 1);
        assert!(matches!(
            ast.types[generic_args[0]].kind,
            TypeExprKind::Uint
        ));
    }
}
