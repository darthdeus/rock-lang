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

#[test]
fn malformed_source_is_rejected_with_its_diagnostic() {
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
        (
            r#"extern(lua, "host..get") fn get() -> int"#,
            "expected a Lua symbol path",
        ),
        (
            r#"extern(lua, "host.end") fn get() -> int"#,
            "expected a Lua symbol path",
        ),
        (
            r#"extern(lua, "host.get") fn get() -> int { 0 }"#,
            "an extern Lua symbol binding cannot have a body",
        ),
        ("struct Box<T { value: int }\n", ""),
        // A hole that fails to parse is never emitted as literal brace text.
        (r#"fn f() { let x = "{add2(2,}" }"#, ""),
        ("fn f() { let x = P { a: 1, ..base } }", ""),
        ("fn f() { let x = E::V { ..base, a: 1 } }", ""),
        ("fn f() { let t = (a : int, b : int) }", "one expression"),
        (
            "fn f(a: int) { let t = (a, b : int) }",
            "parenthesize the ascription",
        ),
        ("fn f() {}\nmod combat;", "first item"),
        ("mod a {}\nmod combat;", "inline module blocks"),
        ("fn f() {}\nmod combat::casting;", "first item"),
        ("fn f() {}\npub mod combat;", "first item"),
        ("mod a { mod b; }", "inline module blocks"),
        ("mod a { mod b::c; }", "inline module blocks"),
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
        ("pub(super) fn f() {}", "unknown visibility qualifier"),
        ("pub(in a) fn f() {}", "unknown visibility qualifier"),
        (
            "fn f() { foo!(1) }",
            "bang-sigil macro calls are not supported",
        ),
        ("foo!(1)", "bang-sigil macro calls are not supported"),
        ("mod child {}", "inline module blocks are not supported"),
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
fn item_recovery_does_not_swallow_the_next_fn() {
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

#[test]
fn qualified_coroutine_method_keeps_its_coroutine_flag() {
    let ast = parse("co fn ns::Actor.step(self) -> Coroutine<int> { yield 1 }");
    assert!(first_fn(&ast).is_coroutine);
}
