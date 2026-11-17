use super::*;

fn file() -> Ustr {
    Ustr::from("test.rock")
}

/// Parse `src` with no recovery allowed.
fn parse(src: &str) -> Ast {
    parse_module_clean(src, file()).unwrap_or_else(|e| panic!("{src:?}: {}", e.message))
}

fn parse_v1(src: &str) -> Ast {
    parse(src)
}

/// Every diagnostic `src` produces, whether the tokenizer or the parser raised it.
fn errors(src: &str) -> Vec<CompilerError> {
    match parse_module(src, file()) {
        Ok(ast) => ast.recovery_errors,
        Err(e) => vec![e],
    }
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

fn body(ast: &Ast) -> &BlockData {
    &ast.blocks[first_fn(ast).body.rock_block().unwrap()]
}

/// The value of the first statement of the first fn, a `let`.
fn value(ast: &Ast) -> Idx<ExprData> {
    let StatementKind::Let(l) = &ast.stmts[body(ast).statements[0]].kind else {
        panic!("expected let")
    };
    l.value.unwrap()
}

fn expression(src: &str) -> (Ast, Idx<ExprData>) {
    let ast = parse(&format!("fn f() {{ let result = {src} }}"));
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

fn fn_names(ast: &Ast) -> Vec<&str> {
    ast.items
        .iter()
        .filter_map(|i| match &i.kind {
            ItemKind::Function(f) => Some(f.name.name.as_str()),
            _ => None,
        })
        .collect()
}

/// Re-parse `full.items[index]` from its text slice, as the incremental driver does.
fn reparse_item(full: &Ast, src: &str, index: usize) -> Ast {
    let item = &full.items[index];
    let (s, e) = item.span.offset_range();
    parse_one_item(
        &src[s..e],
        file(),
        s,
        item.span.line_range().0,
        item.span.col_range().0,
    )
    .expect("parse_one_item")
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
fn malformed_source_is_rejected_with_its_diagnostic() {
    // An empty needle accepts any diagnostic; the point is that the source is
    // not silently accepted.
    for (src, needle) in [
        ("nobox struct S { x: int }", "`nobox` requires `value`"),
        (
            "struct P { x: int }\noperator +(a: P) -> P { a }",
            "operator `+` takes two parameters",
        ),
        (
            "inline(wasm) fn add(a: int) -> int = \"\"\"return a\"\"\"",
            "unknown inline backend",
        ),
        ("extern(wasm) fn now() -> float", "unknown extern backend"),
        ("struct Box<T { value: int }\n", ""),
        // A hole that fails to parse is never emitted as literal brace text.
        (r#"fn f() { let x = "{add2(2,}" }"#, ""),
        // Field before base in a functional-update literal.
        ("fn f() { let x = P { a: 1, ..base } }", ""),
        // Functional update on an enum variant is not supported.
        ("fn f() { let x = E::V { ..base, a: 1 } }", ""),
        ("fn f(x: int) { let y = g(x) :: int }", "(expr : Type)"),
        // `(a : int, b : int)` is not a typed-tuple literal.
        ("fn f() { let t = (a : int, b : int) }", "one expression"),
        (
            "fn f(a: int) { let t = (a, b : int) }",
            "parenthesize the ascription",
        ),
        ("fn f() {}\nmod combat;", "first item"),
        ("mod a {}\nmod combat;", "first item"),
        ("fn f() {}\nmod combat::casting;", "first item"),
        ("fn f() {}\npub mod combat;", "first item"),
        ("mod a { mod b; }", "first item"),
        ("mod a { mod b::c; }", "first item"),
        (
            "mod a;\nmod b;\n\nfn f() {}",
            "a file has one `mod path;` header",
        ),
        (
            "mod a;\n// between\npub mod a;\n",
            "a file has one `mod path;` header",
        ),
        ("use a::b as _;", "as _"),
        ("use geo::operator(+) as _;", "as _"),
        ("private(set) fn f() {}", "fields"),
        ("fn f<T: >() {}", ""),
        ("fn f<T = >() {}", ""),
        (
            "fn f() { let v = \"{1:999999999999999999999999999999999999}\" }",
            "overflow",
        ),
        (
            "fn f() { let v = \"{1:.999999999999999999999999999999999999}\" }",
            "overflow",
        ),
    ] {
        let errors = errors(src);
        assert!(
            errors.iter().any(|e| e.message.contains(needle)),
            "{src:?} must be rejected with {needle:?}, got {errors:?}"
        );
    }
}

#[test]
fn parse_one_item_matches_in_context() {
    // Re-parsing a single item from its text slice (with its absolute
    // offset/line/col) must preserve the item and body spans so the
    // incremental driver can swap it in. An item's span covers its leading
    // annotations, and the multi-line indented item shows a wrong base
    // offset/line/col.
    let src = "fn a() -> int { 1 }\n\
               @test\n\
               fn b(x: int) -> int {\n    \
                   let y = x + 2\n    \
                   y * 3\n\
               }\n\
               fn c() -> int { 9 }\n";
    let full = parse(src);
    for (index, item) in full.items.iter().enumerate() {
        let mini = reparse_item(&full, src, index);
        assert_eq!(mini.items.len(), 1, "exactly one item parsed");
        let m = &mini.items[0];
        assert_eq!(m.span.offset_range(), item.span.offset_range());
        assert_eq!(m.span.line_range(), item.span.line_range());
        assert_eq!(m.annotations.len(), item.annotations.len());
        let mb = first_fn(&mini).body.rock_block().unwrap();
        let ItemKind::Function(f) = &item.kind else {
            panic!("expected fn")
        };
        let fb = f.body.rock_block().unwrap();
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
    assert_eq!(full.items[1].annotations.len(), 1);
}

#[test]
fn parse_module_clean_rejects_what_parse_module_recovers_from() {
    // A fn body-less because its inline body lost its `=`: recovered, the
    // item dropped, and `parse_module` still Ok. The strict form is the
    // oracle a fixture helper needs.
    let src = "inline(lua) fn f(x: int) -> int \"\"\" return x \"\"\"\n";
    let lenient = parse_module(src, file()).expect("recovering parse still returns a tree");
    assert!(
        !lenient.recovery_errors.is_empty(),
        "the dropped item is recorded"
    );
    assert!(
        fn_names(&lenient).is_empty(),
        "the malformed fn is absent from the lenient tree, which is the trap"
    );
    assert!(
        parse_module_clean(src, file()).is_err(),
        "strict parse rejects it"
    );
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
    assert_eq!(
        fn_names(&ast),
        ["after"],
        "the deep fn is dropped and statement recovery still works after it"
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
    assert_eq!(fn_names(&ast), ["helper", "survivor"]);
}

#[test]
fn recovery_preserves_fields_and_following_bindings() {
    let ast = parse_module(
        "fn f() { let p = P { a: 1 b: 2 }\nlet broken =\nlet kept = 3 }",
        file(),
    )
    .unwrap();
    assert!(!ast.recovery_errors.is_empty());
    let ExprKind::PathStructLiteral { fields, .. } = &ast.exprs[value(&ast)].kind else {
        panic!("expected struct")
    };
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].0[0].name, "a");
    assert_eq!(fields[1].0[0].name, "b");
    assert_eq!(body(&ast).statements.len(), 2);
    let StatementKind::Let(l) = &ast.stmts[body(&ast).statements[1]].kind else {
        panic!("expected let")
    };
    assert!(matches!(&l.pattern, Pattern::Ident(i) if i.name == "kept"));
}

#[test]
fn annotations_attach_to_items_params_and_fields() {
    let ast = parse("@pure fn f(@size(4) a: int, @instance @size(2) b: float, c: int) {}");
    assert_eq!(ast.items[0].annotations.len(), 1);
    assert_eq!(ast.items[0].annotations[0].name.name, "pure");
    let params = &first_fn(&ast).params;
    let [a] = &params[0].annotations[..] else {
        panic!("param 0 has one annotation")
    };
    assert_eq!(a.name.name, "size");
    assert!(a.args[0].name.is_none(), "positional arg");
    assert!(matches!(
        &ast.exprs[a.args[0].value].kind,
        ExprKind::Int(4, _)
    ));
    // The annotation span is a real source range distinct from the param name's.
    assert!(a.span.offset_range().0 < a.span.offset_range().1);
    assert_ne!(a.span.offset_range(), params[0].name.span.offset_range());
    let names = |anns: &[Annotation]| -> Vec<(String, usize)> {
        anns.iter()
            .map(|a| (a.name.name.to_string(), a.args.len()))
            .collect()
    };
    assert_eq!(
        names(&params[1].annotations),
        [("instance".into(), 0), ("size".into(), 1)]
    );
    assert!(params[2].annotations.is_empty());

    let ast = parse("struct S { @flat a: int, @w(3) b: float, c: bool }");
    let fields = &first_struct(&ast).fields;
    assert_eq!(names(&fields[0].annotations), [("flat".into(), 0)]);
    assert_eq!(names(&fields[1].annotations), [("w".into(), 1)]);
    assert!(fields[2].annotations.is_empty());

    let ast = parse("nobox value V { @flat @w c: int }");
    let s = first_struct(&ast);
    assert!(s.is_value && s.is_nobox);
    assert_eq!(
        names(&s.fields[0].annotations),
        [("flat".into(), 0), ("w".into(), 0)]
    );
}

#[test]
fn interpolation_format_specs_keep_static_and_dynamic_parts() {
    let spec_of = |src: &str| {
        let (ast, id) = expression(src);
        let ExprKind::InterpolatedString(parts) = &ast.exprs[id].kind else {
            panic!("expected interpolated string, got {:?}", ast.exprs[id].kind);
        };
        let InterpolatedPart::Expr(e, Some(spec)) = &parts[0] else {
            panic!("expected embedded expr with a format spec: {parts:?}");
        };
        let (e, spec) = (*e, spec.clone());
        (ast, e, spec)
    };
    let (_, _, spec) = spec_of(r#""{v:.3}""#);
    assert_eq!(spec.precision, Some(3));

    let (ast, e, spec) = spec_of(r#""{xs[0]:3}""#);
    assert!(matches!(ast.exprs[e].kind, ExprKind::Index(..)));
    assert_eq!(spec.width, Some(3));

    // Embedded width/precision exprs survive as real arena nodes.
    let (ast, _, spec) = spec_of(r#""{v:{w}.{p}f}""#);
    let we = spec.width_expr.expect("width_expr dropped");
    let pe = spec.precision_expr.expect("precision_expr dropped");
    assert!(matches!(&ast.exprs[we].kind, ExprKind::Ident(id) if id.name == "w"));
    assert!(matches!(&ast.exprs[pe].kind, ExprKind::Ident(id) if id.name == "p"));
    assert_eq!(spec.format_type, Some('f'));
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
        let (ast, id) = expression(source);
        let ExprKind::InterpolatedString(parts) = &ast.exprs[id].kind else {
            panic!("expected interpolation")
        };
        let [InterpolatedPart::Expr(e, None)] = parts.as_slice() else {
            panic!("colon became a format spec: {parts:?}")
        };
        assert!(matches!(
            ast.exprs[*e].kind,
            ExprKind::FunctionCall { .. } | ExprKind::PathStructLiteral { .. }
        ));
    }
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
    let full = parse_module(src, file()).expect("full parse");
    for (name, text) in ident_spans(&full, src) {
        assert_eq!(text, name, "ident span does not cover its source");
    }
    // The single-item reparse starts its source mid-file, so a hole offset
    // left relative to that slice lands `offset` bytes short.
    for (name, text) in ident_spans(&reparse_item(&full, src, 1), src) {
        assert_eq!(text, name, "reparsed ident span does not cover its source");
    }
}

#[test]
fn holes_keep_their_names_in_expr_and_type_position() {
    let (ast, v) = expression("?");
    assert!(matches!(ast.exprs[v].kind, ExprKind::Hole(_, None)));
    let (ast, v) = expression("?foo");
    assert!(matches!(&ast.exprs[v].kind, ExprKind::Hole(_, Some(n)) if n == "foo"));
    let ast = parse("fn f() { let x: ?foo = 1 }");
    let StatementKind::Let(l) = &ast.stmts[body(&ast).statements[0]].kind else {
        panic!("expected let")
    };
    let ty = l.ty.expect("no type annotation");
    assert!(matches!(&ast.types[ty].kind, TypeExprKind::Hole(_, Some(n)) if n == "foo"));
}

#[test]
fn keyword_spans_and_dropped_fields_are_captured() {
    let ast = parse("global G: int = 0\nfn f() {\nfor x: int in xs {\nreturn x\n}\n}");
    let ItemKind::Global(g) = &ast.items[0].kind else {
        panic!("expected global")
    };
    assert!(g.keyword_span.is_some(), "global keyword_span dropped");
    let ItemKind::Function(func) = &ast.items[1].kind else {
        panic!("expected fn")
    };
    assert!(func.keyword_span.is_some(), "fn keyword_span dropped");
    let body = func.body.rock_block().unwrap();
    let StatementKind::For(f) = &ast.stmts[ast.blocks[body].statements[0]].kind else {
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
    let ast =
        parse("fn use_player(player: ref Player, maybe: (ref Player)?) -> ref Player { player }");
    let f = first_fn(&ast);
    assert_eq!(ast.type_surface(f.params[0].ty), "ref Player");
    assert_eq!(ast.type_surface(f.params[1].ty), "(ref Player)?");
    assert_eq!(
        ast.type_surface(f.return_ty.expect("return type")),
        "ref Player"
    );
}

#[test]
fn type_surface_preserves_path_roots_and_arguments() {
    for ty in ["::a::B", "a::B", "::a::B<int>", "a::B<int>"] {
        let ast = parse(&format!("fn f(value: {ty}) {{}}"));
        assert_eq!(ast.type_surface(first_fn(&ast).params[0].ty), ty);
    }
}

#[test]
fn sibling_field_ref_in_default() {
    let ast = parse("struct HealthBar { max: int, current: int = .max }");
    let def = first_struct(&ast).fields[1]
        .default
        .expect("current has a default");
    assert!(matches!(&ast.exprs[def].kind, ExprKind::SiblingFieldRef(id) if id.name == "max"));
}

#[test]
fn fn_bodies_record_backend_and_source() {
    for (src, want) in [
        (
            "inline(lua) fn add(a: int, b: int) -> int = \"\"\"return a + b\"\"\"",
            FunctionBody::Inline(FnBackend::Lua, "return a + b".into()),
        ),
        (
            "inline(js) fn add(a: int, b: int) -> int = \"\"\"return a + b\"\"\"",
            FunctionBody::Inline(FnBackend::Js, "return a + b".into()),
        ),
        (
            "inline fn add(a: int) -> int = \"\"\"return a\"\"\"",
            FunctionBody::Inline(FnBackend::Lua, "return a".into()),
        ),
        ("extern(js) fn now() -> float", FunctionBody::ExternJs),
    ] {
        assert_eq!(first_fn(&parse(src)).body, want, "{src}");
    }
}

/// Every block with recorded brace spans must have them sitting on literal
/// `{` / `}` at its own span's ends; blocks without them are exactly the
/// parser's synthetic braceless wrappers (lambda `=> expr`, `@`-trailing
/// lambda, match-arm `=> expr`). Consumers (bone-lower's brace lapper
/// markers) emit nothing for `braces: None` instead of deriving positions
/// from `span` -- which planted phantom 1-wide nodes on the first/last
/// chars of braceless bodies (`bone-query/tests/phantom_block_sentinel.rs`).
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
    let ast = parse(src);
    let mut braceless = 0;
    for (_, b) in ast.blocks.iter() {
        match b.braces {
            Some((open, close)) => {
                let (bs, be) = b.span.offset_range();
                assert_eq!(open.offset_range(), (bs, bs + 1));
                assert_eq!(close.offset_range(), (be - 1, be));
                assert_eq!(&src[bs..bs + 1], "{");
                assert_eq!(&src[be - 1..be], "}");
            }
            None => braceless += 1,
        }
    }
    // Lambda `=> 1`, `@ 3`, match arm `=> 5`.
    assert_eq!(braceless, 3, "synthetic braceless block count");
}

#[test]
fn macro_expanded_blocks_lose_brace_spans() {
    // Span retag stamps the call-site span on expanded template blocks; the
    // template's braces don't exist at the call site, so they must be
    // dropped rather than left pointing at template offsets.
    let mut ast = parse("macro m() { { 1 } }\nfn f() { m() }");
    macro_expand::expand_macros(std::slice::from_mut(&mut ast));
    desugar::desugar(&mut ast);
    let f = ast
        .items
        .iter()
        .find_map(|i| match &i.kind {
            ItemKind::Function(f) if f.name.name == "f" => Some(f),
            _ => None,
        })
        .expect("fn f survives cook");
    let body = f.body.rock_block().unwrap();
    assert!(ast.b(&body).braces.is_some(), "fn body braces survive cook");
    let call_span = ast.stmts[ast.b(&body).statements[0]].span;
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

#[test]
fn item_macros_splice_and_substitute_type_fragments() {
    let mut ast = parse(
        "macro mk(N: ident, E: ty) { struct N { v: E } }\nmk!(WrapBox, Box<int>)\nmk!(WrapArr, [int])",
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

#[test]
fn operators_preserve_precedence_and_left_associativity() {
    let (ast, id) = expression("1 + 2 * 3 - 4");
    let (left, right) = binary(&ast, id, TokenKind::Minus);
    assert!(matches!(ast.exprs[right].kind, ExprKind::Int(4, _)));
    let (_, product) = binary(&ast, left, TokenKind::Plus);
    binary(&ast, product, TokenKind::Mul);
    for (_, expr) in ast.exprs.iter() {
        if let ExprKind::BinOp { op, op_span, .. } = &expr.kind {
            let (start, end) = op_span.offset_range();
            assert_eq!(
                &"fn f() { let result = 1 + 2 * 3 - 4 }"[start..end],
                op.to_code()
            );
        }
    }
    let (ast, id) = expression("a < b && c > d");
    let (left, right) = binary(&ast, id, TokenKind::And);
    binary(&ast, left, TokenKind::LessThan);
    binary(&ast, right, TokenKind::GreaterThan);
    // A leading operator continues the expression across a newline.
    let (ast, id) = expression("10\n - 1\n - 2");
    let (left, _) = binary(&ast, id, TokenKind::Minus);
    binary(&ast, left, TokenKind::Minus);
}

#[test]
fn guarded_match_keeps_all_arms_and_struct_bodies() {
    for guard in ["x", "(x)", "x && x"] {
        let (ast, id) = expression(&format!(
            "match e {{ E::A if {guard} => S {{ x: 1 }} E::B => S {{ x: 2 }} }}"
        ));
        let ExprKind::Match { arms, .. } = &ast.exprs[id].kind else {
            panic!("expected match")
        };
        assert_eq!(arms.len(), 2);
        assert!(arms[0].guard.is_some());
        assert!(arms[1].guard.is_none());
        for arm in arms {
            let StatementKind::Expression(e) = ast.stmts[ast.blocks[arm.body].statements[0]].kind
            else {
                panic!("expected expression")
            };
            assert!(matches!(
                ast.exprs[e].kind,
                ExprKind::PathStructLiteral { .. }
            ));
        }
    }
}

#[test]
fn ascription_and_lambda_disambiguation() {
    let (ast, id) = expression("(5 : int)");
    let ExprKind::ParenAscription(e, ty) = ast.exprs[id].kind else {
        panic!("expected ascription")
    };
    assert!(matches!(ast.exprs[e].kind, ExprKind::Int(5, _)));
    assert!(matches!(ast.types[ty].kind, TypeExprKind::Int));
    let (ast, id) = expression("(x : int) => x");
    assert!(matches!(ast.exprs[id].kind, ExprKind::Lambda { .. }));
    let (ast, id) = expression("(j : float) / (k : float)");
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
fn legacy_infix_ascription_is_rejected() {
    let src = "fn f(x: int) { let y = g(x) :: int }";
    let error = parse_module_clean(src, file()).expect_err("infix ascription must be rejected");
    assert!(error.message.contains("write `(expr : Type)`"), "{error:?}");
    parse_module_clean("fn f(x: int) { let y = (x : int) }", file())
        .expect("parenthesized ascription parses");
}

#[test]
fn removed_legacy_module_directive_is_rejected_with_its_span() {
    let src = "  //! modules: legacy\nfn f() {}\n";
    let error = parse_module(src, file()).expect_err("removed directive must be rejected");
    assert!(
        error.message.contains("directive has been removed"),
        "{error:?}"
    );
    assert_eq!(error.labels[0].span.offset_range(), (2, 21));
    assert_eq!(error.labels[0].span.line_range(), (0, 0));

    let error = parse_module("//! modules: v2\nfn f() {}\n", file())
        .expect_err("unknown directive values must be rejected");
    assert!(
        error
            .message
            .contains("unknown `//! modules:` directive value `v2`"),
        "{error:?}"
    );
}

#[test]
fn paths_keep_segments_root_and_generic_arguments() {
    for (source, root, names) in [
        ("::app::Actor", true, vec!["app", "Actor"]),
        ("self::Actor", false, vec!["self", "Actor"]),
        ("super::super::T", false, vec!["super", "super", "T"]),
        ("mod_u::helper", false, vec!["mod_u", "helper"]),
        // No name-resolution-based guessing: `x :: float` is a two-segment
        // path in v1; whether it names anything is the resolver's call.
        ("x :: float", false, vec!["x", "float"]),
    ] {
        let (ast, id) = expression(source);
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
    let ast = parse_v1("fn f() { let x: native::collections::Vec<int> = x }");
    let StatementKind::Let(l) = &ast.stmts[body(&ast).statements[0]].kind else {
        panic!("expected let")
    };
    let TypeExprKind::AppPath { ctor, args } = &ast.types[l.ty.unwrap()].kind else {
        panic!("expected generic path")
    };
    assert_eq!(ctor.segments.len(), 3);
    assert_eq!(args.len(), 1);
    assert!(matches!(ast.types[args[0]].kind, TypeExprKind::Int));
    let (ast, id) = expression("match msg { msg_kind::Ping => 1, other => 0 }");
    let ExprKind::Match { arms, .. } = &ast.exprs[id].kind else {
        panic!("expected match")
    };
    assert!(
        matches!(&arms[0].pattern, Pattern::PathVariant { path, .. } if path.segments[0].name == "msg_kind" && path.segments[1].name == "Ping")
    );
}

#[test]
fn qualified_paths_head_calls_and_struct_literals() {
    let (ast, id) = expression("sample::combat::heal(actor, 5)");
    let ExprKind::FunctionCall { func, .. } = &ast.exprs[id].kind else {
        panic!("expected call, got {:?}", ast.exprs[id].kind);
    };
    assert!(matches!(ast.exprs[*func].kind, ExprKind::Path { .. }));

    let (ast, id) = expression("p::make<int>(3)");
    let ExprKind::FunctionCall { generic_args, .. } = &ast.exprs[id].kind else {
        panic!("expected call, got {:?}", ast.exprs[id].kind);
    };
    assert_eq!(generic_args.len(), 1, "explicit type argument survived");

    let (ast, id) = expression("lib::Actor { hp: 100 }");
    let ExprKind::PathStructLiteral { path, fields, .. } = &ast.exprs[id].kind else {
        panic!("expected path struct literal, got {:?}", ast.exprs[id].kind);
    };
    assert_eq!(path.segments.len(), 2);
    assert_eq!(fields.len(), 1);

    // Every qualified value is a path; capitalization does not change its AST.
    let (ast, id) = expression("Color::Red");
    assert!(matches!(ast.exprs[id].kind, ExprKind::Path { .. }));

    let ast = parse_v1("fn lib::Actor.heal(a, amount: int) { a.hp = a.hp + amount }");
    let f = first_fn(&ast);
    assert_eq!(f.name.name, "heal");
    assert!(f.receiver_surface.is_some(), "receiver surface recorded");
}

#[test]
fn single_and_multiple_if_let_bindings_use_chains() {
    for (source, count) in [
        ("if let x = foo() { x } else { 0 }", 1),
        ("if let x = foo() && let y = bar() { y }", 2),
    ] {
        let (ast, id) = expression(source);
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
fn file_headers_parse_in_v1() {
    for (src, path, vis) in [
        (
            "mod combat::casting;\n\nfn f() {}",
            vec!["combat", "casting"],
            None,
        ),
        (
            "pub mod combat;\n\nfn f() {}",
            vec!["combat"],
            Some(Visibility::Pub),
        ),
        // `mod self;` is the explicit root.
        ("mod self;\n\nfn f() {}", vec![], None),
    ] {
        let ast = parse_v1(src);
        let h = ast.module_header.as_ref().expect("header recorded");
        let names: Vec<_> = h.path.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, path, "{src}");
        assert_eq!(h.vis, vis, "{src}");
    }
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
        let ast = parse_v1(src);
        assert!(ast.module_header.is_none(), "{src:?}");
        assert!(
            ast.items
                .iter()
                .all(|i| matches!(i.kind, ItemKind::Module(_) | ItemKind::Function(_))),
            "{src:?}"
        );
    }
    assert_eq!(parse_v1("mod a {}\nmod b {}").items.len(), 2);
    let ast = parse_v1("pub mod combat {\n    pub fn heal() {}\n}\n");
    let ItemKind::Module(m) = &ast.items[0].kind else {
        panic!("expected mod item")
    };
    assert_eq!(m.name.name, "combat");
    assert_eq!(m.vis, Some(Visibility::Pub));
    assert_eq!(m.items.len(), 1);
}

/// Visibility leads the item, annotations included (`parse_item` reads the
/// modifiers before the annotations), so an annotated item and a custom
/// decorator can be exported.
#[test]
fn visibility_leads_items_annotations_and_decorators() {
    let spelled = |vis: &Option<Visibility>| match vis {
        Some(Visibility::PubIn(path)) => {
            let path: Vec<&str> = path.iter().map(|i| i.name.as_str()).collect();
            format!("pub(in {})", path.join("::"))
        }
        other => format!("{other:?}"),
    };
    for (src, want) in [
        ("pub fn f() {}", "Some(Pub)"),
        ("private struct S {}", "Some(Private)"),
        ("pub(package) global G: int = 0", "Some(PubPackage)"),
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
        assert_eq!(spelled(&parse_v1(src).items[0].vis), want, "{src}");
    }
    let ast = parse_v1("pub @inline fn f() -> int { 1 }");
    assert_eq!(ast.items[0].annotations.len(), 1);
}

#[test]
fn use_forms_parse_in_v1() {
    for src in [
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
    ] {
        assert!(
            matches!(parse_v1(src).items[0].kind, ItemKind::Use(_)),
            "{src}"
        );
    }
    // Block-level `use` is a statement.
    let ast = parse_v1("fn f() {\n    use a::b::c\n    let x = 1\n}");
    assert!(matches!(
        ast.stmts[body(&ast).statements[0]].kind,
        StatementKind::Use(_)
    ));
}

#[test]
fn use_trees_record_operator_selectors_and_nesting() {
    let use_tree = |src: &str| {
        let ast = parse_v1(src);
        let ItemKind::Use(u) = &ast.items[0].kind else {
            panic!("expected use")
        };
        u.tree.clone()
    };
    let UseTree::Path {
        kind: UseSuffix::Operator { op, alias },
        segments,
        ..
    } = use_tree("use geo::operator(+) as add_pt;")
    else {
        panic!("expected an operator selector");
    };
    assert_eq!(
        segments.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["geo"]
    );
    assert_eq!(op.name, "__op_add");
    assert_eq!(alias.as_ref().map(|a| a.name.as_str()), Some("add_pt"));

    let UseTree::Path {
        kind: UseSuffix::Operator { op, alias },
        ..
    } = use_tree("use geo::operator(!);")
    else {
        panic!("expected an operator selector");
    };
    assert_eq!(op.name, "__not");
    assert!(alias.is_none());

    let UseTree::Path {
        kind: UseSuffix::Nested(trees),
        ..
    } = use_tree("use a::b::{c::{d, e}, f::g as h};")
    else {
        panic!("expected nested");
    };
    assert_eq!(trees.len(), 2);
}

#[test]
fn field_visibility_and_contextual_words_in_v1() {
    let ast = parse_v1(
        "pub struct Actor {\n    name: string,\n    private(set) health: float = 100,\n    pub(package) private(set) counter: int,\n}",
    );
    let fields = &first_struct(&ast).fields;
    assert!(fields[0].vis.is_none());
    assert!(fields[1].set_private);
    assert!(fields[2].set_private);
    assert_eq!(fields[2].vis, Some(Visibility::PubPackage));
    // `private: bool` is a field named `private`; `mod` and `use` do not
    // reserve anything (contextual words only).
    parse_v1(
        "struct S {\n    private: bool,\n    pub: int,\n}\nfn f(p: S) { let x = p\nprint(x) }",
    );
}

#[test]
fn local_declarations_preserve_storage_types_and_nested_functions() {
    let ast = parse(
        "fn f() { static count = 0\nstatic scratch: [int] = []\nfn inner(x: int) -> int { x }\nlet m: Map<int, Vec<int>> = a }",
    );
    let stmts = &body(&ast).statements;
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
    let ast = parse(
        "fn f() { draw(img, ..props, color: c)\nlet p = P { ..a, ..b, c: 1 }\nworkgroup_array<uint>(64) }",
    );
    let stmts = &body(&ast).statements;
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
    let ExprKind::PathStructLiteral { bases, fields, .. } = &ast.exprs[l.value.unwrap()].kind
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

#[test]
fn a_comment_may_close_a_delimited_list() {
    for src in [
        "fn f() {\n    g(\n        a,\n        // c\n    )\n}\n",
        "fn f() {\n    g(\n        x: a,\n        // c\n    )\n}\n",
        "fn f() {\n    x.g<int>(\n        a,\n        // c\n    )\n}\n",
        "fn f() {\n    let v = (\n        a,\n        b,\n        // c\n    )\n}\n",
        "@a(\n    1,\n    // c\n)\nfn f() {}\n",
        "fn f<\n    T,\n    // c\n>() {}\n",
        "enum E {\n    A(\n        int,\n        // c\n    ),\n}\n",
        "fn f() {\n    let l = (\n        // c\n        a: int,\n        // c\n    ) => a\n}\n",
    ] {
        parse(src);
    }
}

#[test]
fn a_comment_may_sit_before_any_list_comma() {
    for src in [
        "fn f() {\n    g(a /* c */, b)\n}\n",
        "fn f() {\n    g(x: a /* c */, y: b)\n}\n",
        "fn f() {\n    let v = [a /* c */, b]\n}\n",
        "@a(1 /* c */, 2)\nfn f() {}\n",
        "fn f(m: Map<int /* c */, int>) {}\n",
        "fn f(m: Map<\n    // c\n    int,\n>) {}\n",
    ] {
        parse(src);
    }
}
