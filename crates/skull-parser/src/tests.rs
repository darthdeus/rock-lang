use super::*;

fn file() -> Ustr {
    Ustr::from("test.rock")
}

fn parse(src: &str) -> Ast {
    parse_module_clean(src, file()).unwrap_or_else(|e| panic!("{src:?}: {}", e.message))
}

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

fn body(ast: &Ast) -> &BlockData {
    &ast.blocks[first_fn(ast).body.rock_block().unwrap()]
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

fn value(ast: &Ast) -> Idx<ExprData> {
    let StatementKind::Let(l) = &ast.stmts[body(ast).statements[0]].kind else {
        panic!("expected let")
    };
    l.value.unwrap()
}

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

#[test]
fn extended_declarations_have_no_syntax_errors() {
    parse(
        r#"mod self;
use a::{b::c, operator(!)};
inline fn native_body() -> int { return 1 }
fn lib::Actor<T>.heal<U>(self, amount: U) -> ::a::B<int> {
    use a::b::c;
    ::app::Actor
}
"#,
    );
}
