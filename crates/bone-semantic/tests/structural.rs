use crate::test_support::{diagnostic_messages, diagnostics, stdlib_modules};
use bone_core::modules::{NsAliases, PackageAliases};
use bone_semantic::prelude::*;
use rock_core::source_code::{IdentityStorage, PackageTag};
use skull_parser::parse_module;

fn parse_rock_with_file(src: &str, file: &str) -> Result<skull_parser::Ast, CompilerError> {
    skull_parser::parse_module_clean(src, Ustr::from(file))
}

fn parse_rock(src: &str) -> Result<skull_parser::Ast, CompilerError> {
    parse_rock_with_file(src, "<parse_rock>")
}

fn check(src: &str) -> Result<Workspace, CompilerError> {
    let mut module = [parse_rock(src)?];
    bone_semantic::semantic_passes::semantic_pass_enforcing_identity_storage(
        &mut module,
        &NsAliases::default(),
    )
}

fn tag(id: u32, self_name: &str, identity_storage: IdentityStorage) -> PackageTag {
    PackageTag {
        id,
        self_name: self_name.to_string(),
        identity_storage,
        root: String::new(),
    }
}

fn parse_in(src: &str, file: &str, package: (u32, &str)) -> skull_parser::Ast {
    let mut module = parse_module(src, Ustr::from(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
    module.package = Some(tag(package.0, package.1, Default::default()));
    module
}

fn aliases(packages: &[(u32, &str, &[(&str, u32)])]) -> NsAliases {
    NsAliases {
        packages: packages
            .iter()
            .map(|(id, name, deps)| PackageAliases {
                id: *id,
                self_name: Ustr::from(name),
                aliases: deps.iter().map(|(n, id)| (Ustr::from(n), *id)).collect(),
            })
            .collect(),
    }
}

macro_rules! named {
    ($arena:expr, $name:expr) => {
        $arena
            .iter()
            .find(|(_, item)| item.name.as_str().rsplit("::").next() == Some($name))
            .unwrap_or_else(|| panic!("`{}` exists", $name))
            .0
    };
}

fn user_mid(ws: &Workspace) -> ModuleId {
    ws.source_modules().next().expect("one user module").id
}

fn module_by_file(ws: &Workspace, file: &str) -> ModuleId {
    ws.modules
        .iter()
        .find(|m| m.file == ustr(file))
        .unwrap_or_else(|| panic!("module {file}"))
        .id
}

fn fn_owner(ws: &Workspace, name: &str) -> BodyOwner {
    BodyOwner::Function(named!(ws.module(user_mid(ws)).functions, name))
}

fn fn_body<'a>(ws: &'a Workspace, name: &str) -> &'a Body {
    &ws.module(user_mid(ws)).bodies[&fn_owner(ws, name)]
}

fn fn_errors<'a>(ws: &'a Workspace, name: &str) -> &'a [CompilerError] {
    ws.inference(user_mid(ws))
        .expect("user module inferred")
        .errors
        .get(&fn_owner(ws, name))
        .map_or(&[], Vec::as_slice)
}

fn text<'a>(src: &'a str, span: &Span) -> &'a str {
    let (s, e) = span.offset_range();
    &src[s..e]
}

#[test]
fn an_unresolved_v1_variant_pattern_is_labeled_at_its_path() {
    let src =
        "fn f(n: int) -> int {\n    match n {\n        Nope::A => 1,\n        _ => 0,\n    }\n}";
    let module = parse_in(src, "app.rock", (1, "app"));
    let ws = semantic_pass_with_aliases(&mut [module], &aliases(&[(1, "app", &[])]))
        .expect("semantic pass");
    let err = ws
        .modules
        .iter()
        .flat_map(|m| m.lowering_errors.values().flatten())
        .find(|e| e.message.contains("did not resolve"))
        .expect("the unbound pattern prefix is a lowering error");
    let label = err
        .labels
        .first()
        .expect("the unresolved pattern is labeled");
    assert_eq!(label.span.file, Ustr::from("app.rock"));
    assert_eq!(text(src, &label.span), "Nope::A");
}

// Builtin types are interned without a span, so only a user type gets an origin label.
#[test]
fn type_mismatch_labels_the_expected_type_origin() {
    let src = "struct Foo { x: int }\nfn main() { let a: Foo = \"hello\" }";
    let ws = check(src).expect("semantic_pass");
    let err = fn_errors(&ws, "main")
        .iter()
        .find(|e| e.message.contains("expected `Foo`"))
        .expect("a type-mismatch error");
    assert!(
        err.labels.iter().any(|l| text(src, &l.span) == "Foo"),
        "expected a label at the `Foo` annotation; labels: {:?}",
        err.labels
    );
}

#[test]
fn diagnostics_label_the_offending_expression() {
    for (src, fn_name, message, labeled) in [
        (
            "enum E { V { n: int } }\nfn impure() -> int { 1 }\n@pure fn f() -> E { E::V { n: impure() } }",
            "f",
            "not provably pure",
            "impure()",
        ),
        (
            "@pure @inline operator(lua) /(a: float, b: float) -> float = \"return a / b\"\n\
             @pure @inline operator(lua) *(a: float, b: float) -> float = \"return a * b\"\n\
             fn main() { let x: int = 2.0 * (3 / 4) }",
            "main",
            "./",
            "(3 / 4)",
        ),
        ("fn f() -> string {\n    let y = 1\n    y\n}", "f", "", "y"),
        (
            "fn name(a: int) -> string {\n    return \"\"\n}\nfn f() -> string {\n    return \"x {name}\"\n}",
            "f",
            "cannot interpolate",
            "name",
        ),
        (
            "fn u() {\n}\nfn f() -> string {\n    return \"x {u()}\"\n}",
            "f",
            "cannot interpolate",
            "u()",
        ),
        (
            "fn f(value: any) -> string { return \"{value}\" }",
            "f",
            "cannot interpolate",
            "value",
        ),
        (
            "fn f(value: any) -> string { return \"{value:.2f}\" }",
            "f",
            "cannot interpolate",
            "value",
        ),
        (
            "fn f(value: any?) -> string { return \"{value}\" }",
            "f",
            "cannot interpolate",
            "value",
        ),
    ] {
        let ws = check(src).expect("recovered, not aborted");
        let err = fn_errors(&ws, fn_name)
            .iter()
            .find(|e| e.message.contains(message))
            .unwrap_or_else(|| panic!("{src}: no `{message}` error"));
        let label = err.labels.first().expect("labeled error");
        assert_eq!(text(src, &label.span), labeled, "{src}");
    }
}

#[test]
fn missing_return_value_says_so_instead_of_unifying_unit() {
    let src = "fn f() -> string {\n    let y = 1\n    let z = 2\n}";
    let ws = check(src).expect("recovered, not aborted");
    let err = fn_errors(&ws, "f").first().expect("a return-type error");
    let span = err.labels.first().expect("labeled error").span;
    assert!(
        text(src, &span).len() < src.len(),
        "underlines the whole fn"
    );

    let src = "fn f() -> string {\n    let w = undefined_symbol_here\n    let z = 1\n}";
    let ws = check(src).expect("recovered, not aborted");
    let errs = diagnostics(&ws);
    assert!(
        errs.iter()
            .any(|e| e.message.contains("undefined_symbol_here")),
        "the undefined-global error was masked: {errs:?}"
    );
}

#[test]
fn nested_interpolation_does_not_leak_into_outer_parts() {
    use bone_semantic::hir::{Expr, InterpPart};
    let ws = check(
        "fn f(x: int, ok: bool) -> string {\n    \"a: {if ok { \"{x}\" } else { \"no\" }}\"\n}",
    )
    .unwrap();
    let fbody = fn_body(&ws, "f");
    let body = fbody.expr_arena[fbody.root.expect("rock function has a body")];
    let Expr::Block {
        tail: Some(tail), ..
    } = body
    else {
        panic!("function body should be a block with a tail, got {body:?}");
    };
    let Expr::InterpolatedString(parts) = fbody.expr_arena[tail] else {
        panic!("tail should be an interpolated string");
    };
    let parts: Vec<_> = parts.map(|p| fbody.interp_part_arena[p]).collect();
    assert!(
        matches!(
            parts[..],
            [InterpPart::Literal(s), InterpPart::Value { expr, .. }]
                if s == "a: " && matches!(fbody.expr_arena[expr], Expr::If { .. })
        ),
        "inner interpolation parts leaked into the outer range: {parts:?}"
    );
}

#[test]
fn body_type_errors_do_not_abort_other_bodies() {
    let ws = check(
        r#"struct __DecoTarget { name: string, params: [string], func: any }
struct Cfg { scale: int = "nope" }
global LIMIT: int = "too many"
deco init broken_deco {
    let s: int = target.name
}
fn broken_one() -> int { return true }
fn good() -> int { return 3 }
fn broken_two() -> bool { return 42 }"#,
    )
    .expect("body-level type errors must not abort semantic_pass");
    let mid = user_mid(&ws);
    let hir = ws.module(mid);
    let inf = ws.inference(mid).expect("user module inferred");
    let mut broken = vec![
        fn_owner(&ws, "broken_one"),
        fn_owner(&ws, "broken_two"),
        BodyOwner::Global(named!(hir.globals, "LIMIT")),
        BodyOwner::StructDefaults(named!(hir.structs, "Cfg")),
        BodyOwner::Decorator(named!(hir.decorators, "broken_deco")),
    ];
    let mut errored: Vec<_> = inf.errors.keys().copied().collect();
    broken.sort_by_key(|o| format!("{o:?}"));
    errored.sort_by_key(|o| format!("{o:?}"));
    assert_eq!(errored, broken);
    assert!(
        inf.per_fn.contains_key(&fn_owner(&ws, "good")),
        "healthy body must have inference even when sibling bodies are broken"
    );
}

fn arena_borrow_errors(body: &str) -> Vec<String> {
    let mut modules = stdlib_modules();
    let src = format!(
        "value BorrowCell {{ n: int }}\nstruct BorrowRow {{ cell: BorrowCell }}\n\
         global CELLS: Arena<BorrowCell> = arena_new()\n\
         global ROWS: Arena<BorrowRow> = arena_new()\n\
         fn invalidate(h: Entity<BorrowCell>) {{ CELLS.remove(h) }}\n\
         fn consume(r: ref BorrowCell, h: Entity<BorrowCell>) {{ invalidate(h); r.n = 1 }}\n\
         fn later(r: ref BorrowCell, ignored: BorrowCell?) {{ r.n = 1 }}\n\
         extern(lua) fn opaque()\n{body}"
    );
    modules.push(parse_rock_with_file(&src, "borrow.rock").unwrap());
    diagnostic_messages(&semantic_pass(&mut modules).expect("valid Arena borrow fixture"))
}

#[test]
fn arena_value_borrows_reject_detected_invalidation() {
    const HELD: &str = "global HELD: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 0 })\n";
    const RECURSIVE: &str = "fn one(n: int) -> ref BorrowCell { if n == 0 { return CELLS.borrow(HELD).unwrap() }; two(n - 1) }\n\
        fn two(n: int) -> ref BorrowCell { one(n) }\n\
        fn forward<T>(r: ref T) -> ref T { r }\n";
    let mut bodies: Vec<String> = [
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); CELLS.remove(h); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); invalidate(h); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { consume(CELLS.borrow(h).unwrap(), h) }",
        "fn probe(h: Entity<BorrowCell>) { later(CELLS.borrow(h).unwrap(), CELLS.remove(h)) }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); let alias: ref BorrowCell = r; invalidate(h); alias.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); r = CELLS.borrow(h).unwrap(); invalidate(h); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); opaque(); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let f = () => { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); invalidate(h); r.n = 1 }; f() }",
        "fn probe(h: Entity<BorrowCell>) { fn nested(k: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(k).unwrap(); invalidate(k); r.n = 1 } nested(h) }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); let f = () => invalidate(h); f(); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); fn nested(k: Entity<BorrowCell>) { invalidate(k) } nested(h); r.n = 1 }",
    ]
    .map(String::from)
    .into();
    bodies.extend([
        "fn hidden() -> ref BorrowCell { return CELLS.borrow(HELD).unwrap() }\nfn probe() { let r: ref BorrowCell = hidden(); CELLS.remove(HELD); r.n = 1 }",
        "fn hidden(flag: bool) -> ref BorrowCell { if flag { return CELLS.borrow(HELD).unwrap() }; CELLS.borrow(HELD).unwrap() }\nfn probe() { let r: ref BorrowCell = hidden(true); CELLS.remove(HELD); r.n = 1 }",
    ].map(|b| format!("{HELD}{b}")));
    bodies.extend(["one(1)", "two(1)", "forward(one(1))"].map(|origin| {
        format!("{HELD}{RECURSIVE}fn probe() {{ let r: ref BorrowCell = {origin}; CELLS.remove(HELD); r.n = 1 }}")
    }));
    let missed: Vec<_> = bodies
        .iter()
        .map(|body| (body, arena_borrow_errors(body)))
        .filter(|(_, errors)| !errors.iter().any(|e| e.contains("Arena value borrow")))
        .collect();
    assert!(missed.is_empty(), "missed Arena invalidation: {missed:?}");
}

#[test]
fn arena_value_borrows_across_indirect_invalidation() {
    const HOST: &str = r#"
@lua_named struct Host { flag: int }
global HELD: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 10 })
global REPLACEMENT: Entity<BorrowCell>? = null
@nomangle fn replace_held() {
    HELD.remove()
    REPLACEMENT = CELLS.insert(BorrowCell { n: 20 })
}
inline(lua) fn make_host() -> Host = """
    local function replace()
        replace_held()
        return 0
    end
    return setmetatable({}, { __index = replace, __newindex = replace })
"""
"#;
    let cases = [
        (
            "guarded compound operator",
            r#"
global HELD: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 0 })
operator +(a: BorrowCell, b: int) -> BorrowCell { invalidate(HELD); BorrowCell { n: b } }
fn probe(row: BorrowRow?) {
    let r: ref BorrowCell = CELLS.borrow(HELD).unwrap()
    row?.cell += 1
    r.n = 1
}
"#,
        ),
        (
            "resume",
            r#"
co fn worker(h: Entity<BorrowCell>) -> Coroutine<int> { invalidate(h); yield 0 }
fn probe(h: Entity<BorrowCell>) {
    let c = worker(h)
    let r: ref BorrowCell = CELLS.borrow(h).unwrap()
    c.resume()
    r.n = 1
}
"#,
        ),
        (
            "parameter default",
            r#"
global HELD: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 0 })
fn probe(i: int = {
    let r: ref BorrowCell = CELLS.borrow(HELD).unwrap()
    invalidate(HELD)
    r.n = 1
    0
}) {}
"#,
        ),
        (
            "iterator",
            r#"
struct Remover { h: Entity<BorrowCell> }
fn next(it: Remover) -> (bool, int) { invalidate(it.h); (false, 0) }
fn probe(h: Entity<BorrowCell>) {
    let r: ref BorrowCell = CELLS.borrow(h).unwrap()
    for _ in (Remover { h: h }) {}
    r.n = 1
}
"#,
        ),
        (
            "defer in nested block",
            r#"
fn probe(h: Entity<BorrowCell>) {
    let r: ref BorrowCell = CELLS.borrow(h).unwrap()
    if h.get() != null { defer invalidate(h) }
    r.n = 1
}
"#,
        ),
        (
            "defer stack",
            r#"
fn probe(h: Entity<BorrowCell>) {
    let r: ref BorrowCell = CELLS.borrow(h).unwrap()
    defer { r.n = 1 }
    defer invalidate(h)
}
"#,
        ),
        (
            "match guard fallback",
            r#"
fn retire(h: Entity<BorrowCell>) -> bool { invalidate(h); false }
fn probe(h: Entity<BorrowCell>) {
    let r: ref BorrowCell = CELLS.borrow(h).unwrap()
    match 0 {
        0 if retire(h) => (),
        _ => { r.n = 1 },
    }
}
"#,
        ),
        (
            "ref returned through a defer",
            r#"
fn probe(h: Entity<BorrowCell>) -> ref BorrowCell {
    defer invalidate(h)
    return CELLS.borrow(h).unwrap()
}
"#,
        ),
        (
            "ref returned through a defer by a function tail",
            r#"
fn probe(h: Entity<BorrowCell>) -> ref BorrowCell {
    defer invalidate(h)
    CELLS.borrow(h).unwrap()
}
"#,
        ),
        (
            "struct default effect",
            r#"
global HELD: Entity<BorrowCell>? = null
fn retire_default() -> int { invalidate(HELD.unwrap()); 0 }
struct Payload { i: int = retire_default() }
fn probe(h: Entity<BorrowCell>) {
    HELD = h
    let r: ref BorrowCell = CELLS.borrow(h).unwrap()
    let _ = Payload {}
    r.n = 1
}
"#,
        ),
        (
            "erased host property",
            r#"
@lua_named struct Host { flag: int? }
global HELD: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 10 })
@nomangle fn replace_held() { HELD.remove(); CELLS.insert(BorrowCell { n: 20 }) }
inline(lua) fn arm_host(host: any) = """
    setmetatable(host, { __index = function() replace_held(); return 0 end })
"""
fn probe() {
    let host = Host { flag: null }
    let erased: any = host
    arm_host(erased)
    let r: ref BorrowCell = CELLS.borrow(HELD).unwrap()
    let _ = host.flag
    r.n = 99
}
"#,
        ),
        (
            "host global property",
            r#"
@lua_named struct Host { flag: int? }
global HELD: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 10 })
global HOST: Host = Host { flag: null }
@nomangle fn replace_held() { HELD.remove(); CELLS.insert(BorrowCell { n: 20 }) }
inline(lua) fn arm_host() = """
    local host = __rock_global_get(assert(__rock_global_entry("HOST")))
    setmetatable(host, { __index = function() replace_held(); return 0 end })
"""
fn probe() {
    arm_host()
    let r: ref BorrowCell = CELLS.borrow(HELD).unwrap()
    let _ = HOST.flag
    r.n = 99
}
"#,
        ),
        (
            "host record inside a global enum",
            r#"
@lua_named struct Host { flag: int? }
global FIRST: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 10 })
global REPLACEMENT: Entity<BorrowCell>? = null
enum HostContainer { Wrapped(Host) }
global HOST: HostContainer = HostContainer::Wrapped(Host { flag: null })

@nomangle fn replace_held() {
    FIRST.remove()
    REPLACEMENT = CELLS.insert(BorrowCell { n: 20 })
}

inline(lua) fn arm_host() = """
    local record = __rock_global_get(assert(__rock_global_entry("HOST")))
    local host = nil
    for k, v in pairs(record) do
        if type(v) == "table" then host = v end
    end
    assert(host, "missing record payload")
    setmetatable(host, { __index = function()
        replace_held()
        return 0
    end })
"""

fn probe() {
    arm_host()
    let r: ref BorrowCell = CELLS.borrow(FIRST).unwrap()
    match HOST { HostContainer::Wrapped(host) => { let _ = host.flag; r.n = 99 } }
    print(FIRST.get() == null)
    print(REPLACEMENT.unwrap().get().unwrap().n)
}
"#,
        ),
        (
            "global initializer",
            r#"
global HELD: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 0 })
global PROBE: int = {
    let r: ref BorrowCell = CELLS.borrow(HELD).unwrap()
    invalidate(HELD)
    r.n = 1
    0
}
"#,
        ),
    ];
    let property_cases = [
        "let _ = host.flag",
        "host.flag = 1",
        "let _ = table_get(host, \"flag\")",
        "table_set(host, \"flag\", 1)",
        "match host { Host { flag: 0 } => (), _ => () }",
    ].map(|access| {
        let body = format!("{HOST}fn probe() {{ let host = make_host(); let r: ref BorrowCell = CELLS.borrow(HELD).unwrap(); {access}; r.n = 99 }}");
        (access, body)
    });
    let missed: Vec<_> = cases
        .into_iter()
        .map(|(name, body)| (name, body.to_string()))
        .chain(property_cases)
        .filter_map(|(name, body)| {
            let errors = arena_borrow_errors(&body);
            (!errors
                .iter()
                .any(|error| error.contains("Arena value borrow")))
            .then_some((name, errors))
        })
        .collect();
    assert!(missed.is_empty(), "missed Arena invalidation: {missed:#?}");
    let plain = format!(
        "{HOST}fn probe() {{ let host = Host {{ flag: 0 }}; let r: ref BorrowCell = CELLS.borrow(HELD).unwrap(); let _ = host.flag; r.n = 99 }}"
    );
    assert_eq!(arena_borrow_errors(&plain), Vec::<String>::new());
}

#[test]
fn arena_value_borrows_track_foreign_collection_origins() {
    const HOST: &str = r#"
global HELD: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 10 })
@nomangle fn replace_held() { HELD.remove(); CELLS.insert(BorrowCell { n: 20 }) }

inline(lua) fn make_host() -> Map<string, int> = """
    return setmetatable({}, { __index = function() replace_held(); return 0 end })
"""

inline(lua) fn arm_host(map: Map<string, int>) = """
    setmetatable(map, { __index = function() replace_held(); return 0 end })
"""

@lua_named struct HostMap { map: Map<string, int> }
inline(lua) fn arm_record(record: HostMap) = """
    setmetatable(record.map, { __index = function() replace_held(); return 0 end })
"""
inline(lua) fn arm_callback(get_map: fn() -> Map<string, int>) = """
    setmetatable(get_map(), { __index = function() replace_held(); return 0 end })
"""

fn assign(record: HostMap, map: Map<string, int>) { record.map = map }
inline(lua) fn arm_nested(outer: Map<string, Map<string, int>>) = """
    setmetatable(outer.nested, { __index = function() replace_held(); return 0 end })
"""

global ESCAPED: Map<string, int> = map_new()
fn publish(map: Map<string, int>) { ESCAPED = map }
fn arm(map: Map<string, int>) { arm_host(map) }
fn forward(map: Map<string, int>) -> Map<string, int> { map }
fn read(map: Map<string, int>) -> int { map["flag"] }
fn read_vec(vec: [int]) -> int { vec[0] }
"#;
    for (setup, access, rejected) in [
        ("let map = make_host()", r#"let _ = map["flag"]"#, true),
        (
            "let map = make_host(); let alias = map",
            r#"let _ = alias.get("flag")"#,
            true,
        ),
        ("let map = make_host()", "let _ = read(map)", true),
        (
            "let map: Map<string, int> = map_new(); let alias = map; arm_host(alias)",
            r#"let _ = map["flag"]"#,
            true,
        ),
        (
            "let map: Map<string, int> = map_new(); let alias = forward(map); arm(alias)",
            r#"let _ = map["flag"]"#,
            true,
        ),
        (
            "let map: Map<string, int> = map_new(); let record = HostMap { map: map }; arm_record(record)",
            r#"let _ = map["flag"]"#,
            true,
        ),
        (
            "let map: Map<string, int> = map_new(); arm_callback(() => map)",
            r#"let _ = map["flag"]"#,
            true,
        ),
        (
            "let map: Map<string, int> = map_new(); let record = HostMap { map: map_new() }; record.map = map; arm_record(record)",
            r#"let _ = map["flag"]"#,
            true,
        ),
        (
            "let map: Map<string, int> = map_new(); publish(map); arm_host(ESCAPED)",
            r#"let _ = map["flag"]"#,
            true,
        ),
        (
            "let map: Map<string, int> = map_new(); let record = HostMap { map: map_new() }; assign(record, map); arm_record(record)",
            r#"let _ = map["flag"]"#,
            true,
        ),
        (
            r#"let map: Map<string, int> = map_new(); let outer: Map<string, Map<string, int>> = map_new(); outer.insert("nested", map); arm_nested(outer)"#,
            r#"let _ = map["flag"]"#,
            true,
        ),
        (
            r#"let map: Map<string, int> = map_new(); map["flag"] = 0"#,
            r#"let _ = read(map); let _ = map.get("flag")"#,
            false,
        ),
        ("let xs = [0]", "let _ = read_vec(xs)", false),
    ] {
        let body = format!(
            "{HOST}fn probe() {{ {setup}; let r: ref BorrowCell = CELLS.borrow(HELD).unwrap(); {access}; r.n = 99 }}"
        );
        let errors = arena_borrow_errors(&body);
        assert_eq!(
            errors
                .iter()
                .any(|error| error.contains("Arena value borrow")),
            rejected,
            "{setup}; {access}: {errors:?}",
        );
        if !rejected {
            assert!(errors.is_empty(), "{errors:?}");
        }
    }
    let later_escape = format!(
        r#"{HOST}
fn probe() {{
    let map: Map<string, int> = map_new()
    map["flag"] = 0
    let r: ref BorrowCell = CELLS.borrow(HELD).unwrap()
    let _ = map["flag"]
    r.n = 99
    arm_host(map)
}}
"#
    );
    let errors = arena_borrow_errors(&later_escape);
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn arena_borrow_checks_preserve_separate_storage_and_fresh_reborrows() {
    for body in [
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); CELLS.insert(BorrowCell { n: 2 }); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); CELLS.clear(); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); CELLS.remove(h); let fresh = CELLS.insert(BorrowCell { n: 2 }); r = CELLS.borrow(fresh).unwrap(); r.n = 1 }",
        "fn probe(h: Entity<BorrowRow>) { let r: ref BorrowRow = ROWS.borrow(h).unwrap(); ROWS.remove(h); r.cell.n = 1 }",
        "fn probe(h: Entity<BorrowRow>) { let r: ref BorrowCell = ROWS.borrow(h).unwrap().cell; ROWS.remove(h); r.n = 1 }",
        "fn probe(h: Entity<BorrowRow>) { let row = h.borrow().unwrap(); opaque(); row.cell.n = 1 }",
        "fn probe() { let cell = BorrowCell { n: 0 }; let r: ref BorrowCell = cell; opaque(); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); let cell = BorrowCell { n: 0 }; r = cell; CELLS.remove(h); r.n = 1 }",
        "fn stable(owner: ref BorrowRow) -> ref BorrowCell { return owner.cell }\nfn probe() { let owner = BorrowRow { cell: BorrowCell { n: 0 } }; let r: ref BorrowCell = stable(owner); opaque(); r.n = 1 }",
        "global HELD: Entity<BorrowRow> = ROWS.insert(BorrowRow { cell: BorrowCell { n: 0 } })\nfn stable() -> ref BorrowCell { return ROWS.borrow(HELD).unwrap().cell }\nfn probe() { let r: ref BorrowCell = stable(); ROWS.remove(HELD); opaque(); r.n = 1 }",
        "global HELD: Entity<BorrowCell> = CELLS.insert(BorrowCell { n: 0 })\nfn stable(owner: ref BorrowRow) -> ref BorrowCell { fn unused() -> int { return CELLS.borrow(HELD).unwrap().n } return owner.cell }\nfn probe() { let owner = BorrowRow { cell: BorrowCell { n: 0 } }; let r: ref BorrowCell = stable(owner); opaque(); r.n = 1 }",
        "fn touch(r: ref BorrowCell, f: fn()) { r.n = 1 }\nfn probe(h: Entity<BorrowCell>) { touch(CELLS.borrow(h).unwrap(), () => invalidate(h)) }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); let f = () => invalidate(h); r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); fn nested(k: Entity<BorrowCell>) { invalidate(k) } r.n = 1 }",
        "fn probe(h: Entity<BorrowCell>) { invalidate(h); let f = () => { let r: ref BorrowCell = CELLS.borrow(h).unwrap(); r.n = 1 }; f() }",
        "fn probe(h: Entity<BorrowCell>) { invalidate(h); fn nested(k: Entity<BorrowCell>) { let r: ref BorrowCell = CELLS.borrow(k).unwrap(); r.n = 1 } nested(h) }",
        "fn one(n: int, r: ref BorrowCell) -> ref BorrowCell { if n == 0 { return r }; two(n - 1, r) }\nfn two(n: int, r: ref BorrowCell) -> ref BorrowCell { one(n, r) }\nfn forward<T>(r: ref T) -> ref T { r }\nfn probe() { let cell = BorrowCell { n: 0 }; let a: ref BorrowCell = forward(one(1, cell)); let b: ref BorrowCell = two(1, cell); opaque(); a.n = 1; b.n = 1 }",
        r#"
value OuterA { cell: BorrowCell }
value OuterB { cell: BorrowCell }
global AA: Arena<OuterA> = arena_new()
global BB: Arena<OuterB> = arena_new()
fn remove_a_then_use(r: ref BorrowCell, h: Entity<OuterA>) { AA.remove(h); r.n = 1 }
fn probe(ha: Entity<OuterA>, hb: Entity<OuterB>, flag: bool) {
    let a: ref BorrowCell = AA.borrow(ha).unwrap().cell
    let b: ref BorrowCell = BB.borrow(hb).unwrap().cell
    let unused: ref BorrowCell = if flag { a } else { b }
    remove_a_then_use(b, ha)
}
"#,
    ] {
        let errors = arena_borrow_errors(body);
        assert!(errors.is_empty(), "{body}: {errors:?}");
    }
}

#[test]
fn nullable_operator_diagnostic_preserves_accessibility_precedence() {
    let files = crate::test_support::parse_embedded_files(
        r#"
//-- lib.rock @pkg=lib,1 @v1
pub value Number { pub n: int }
pub operator *(a: Number, b: int) -> int { b }
operator *(a: Number?, b: int) -> int { b }
//-- app.rock @pkg=app,2 @v1
use lib::{Number, operator(*)};
fn multiply(x: Number?, y: int) -> int { x * y }
"#,
    );
    let error = crate::test_support::check_types_embedded(
        &files,
        &[(
            "<stdlib>/operators.rock",
            "operator *(a: float, b: float) -> float { a }",
        )],
        &[],
    )
    .unwrap_err();
    assert!(
        error.contains("matching overload is not accessible"),
        "{error}"
    );
}

const ACTOR: &str = "pub struct Actor {\n    pub health: int,\n    pub private(set) mana: int,\n    private secret: int,\n}";

#[test]
fn structural_interface_conformance_rejects_private_set_field_for_writable_interface() {
    use bone_semantic::inference::coerce_to_interface;

    let lib_mod = parse_in(ACTOR, "lib.rock", (1, "game"));
    let consumer_mod = parse_in(
        "pub struct WritableMana { pub mana: int }\n\
         pub struct WritableHealth { pub health: int }\n\
         pub struct MissingField { pub missing: int }\n\
         pub struct ReadableSecret { pub secret: int }",
        "consumer.rock",
        (2, "consumer"),
    );
    let mut ws = semantic_pass_with_aliases(
        &mut [lib_mod, consumer_mod],
        &aliases(&[(1, "game", &[]), (2, "consumer", &[("game", 1)])]),
    )
    .expect("semantic pass");
    for iface in [
        "WritableMana",
        "WritableHealth",
        "MissingField",
        "ReadableSecret",
    ] {
        ws.interface_names
            .insert(ustr(&format!("consumer::{iface}")));
    }

    let body = Body::default();
    let mut gamma = Gamma::new(&ws, &body, module_by_file(&ws, "consumer.rock"));
    let actor_ty = gamma.nominal_app(ustr("game::Actor"), &[], Span::unknown());
    let mut coerce = |iface: &str| {
        coerce_to_interface(
            &mut gamma,
            actor_ty,
            ustr(&format!("consumer::{iface}")),
            Span::unknown(),
        )
        .map_err(|e| e.message)
    };

    assert_eq!(coerce("WritableHealth"), Ok(()));
    assert_eq!(
        coerce("WritableMana").unwrap_err(),
        "field `mana` on struct `Actor` is not writable from this module"
    );
    assert_eq!(
        coerce("MissingField").unwrap_err(),
        "type game::Actor does not have required field `missing` of interface `consumer::MissingField`"
    );
    assert_eq!(
        coerce("ReadableSecret").unwrap_err(),
        "field `secret` on struct `Actor` is not accessible from this module"
    );
}

fn identity_storage_errors(
    std_file: &str,
    std_src: &str,
    package: Option<&str>,
    user_src: &str,
) -> Vec<String> {
    let std_module = parse_rock_with_file(std_src, std_file).expect("parse stdlib module");
    let mut user_module = parse_rock_with_file(user_src, "main.rock").expect("parse user module");
    user_module.package = package.map(|name| tag(1, name, Default::default()));
    diagnostic_messages(&semantic_pass(&mut [std_module, user_module]).expect("semantic pass"))
}

const MINI_ARENA_STORE: &str = "struct ArenaStore<T> {\n    generations: [int],\n    @identity_storage\n    data: [T],\n}\nstruct PlainStore<T> { data: [T] }";

#[test]
fn identity_storage_edge_admits_only_direct_rows() {
    let arena = "<stdlib>/arena.rock";
    let admitted = identity_storage_errors(
        arena,
        MINI_ARENA_STORE,
        Some("game"),
        "struct Mob { hp: int }\nglobal MOBS: ArenaStore<Mob> = ArenaStore { generations: [], data: [] }",
    );
    assert!(admitted.is_empty(), "{admitted:?}");

    for user in [
        "struct Mob { hp: int }\nglobal MOBS: ArenaStore<[Mob]> = ArenaStore { generations: [], data: [] }",
        "struct Mob { hp: int }\nglobal MOBS: PlainStore<Mob> = PlainStore { data: [] }",
    ] {
        let errors = identity_storage_errors(arena, MINI_ARENA_STORE, Some("game"), user);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("cannot store identity struct `game::Mob`")),
            "{user}: {errors:?}"
        );
    }
}

#[test]
fn identity_storage_capability_is_the_arena_files_alone() {
    let forged = identity_storage_errors(
        "<stdlib>/scene.rock",
        MINI_ARENA_STORE,
        Some("game"),
        "fn main() {}",
    );
    assert!(
        forged
            .iter()
            .any(|e| e.contains("`@identity_storage` is reserved")),
        "another stdlib file cannot claim the Arena row store: {forged:?}"
    );
    let not_vec = identity_storage_errors(
        "<stdlib>/arena.rock",
        "struct ArenaStore<T> {\n    @identity_storage\n    row: T,\n}",
        Some("game"),
        "fn main() {}",
    );
    assert!(
        not_vec.iter().any(|e| e.contains("marks the Vec field")),
        "the capability names the row Vec: {not_vec:?}"
    );
    let map_field = identity_storage_errors(
        "<stdlib>/arena.rock",
        MINI_ARENA_STORE,
        Some("game"),
        "struct Mob { hp: int }\nstruct Rows {\n    @identity_storage\n    by_id: Map<int, Mob>,\n}",
    );
    assert_eq!(
        map_field,
        ["`@identity_storage` is reserved for the standard arena row store"],
    );
}

#[test]
fn identity_storage_exemption_follows_the_package_declaration() {
    let user = "struct Mob { hp: int }\nstruct Room { mobs: [Mob] }\nfn count(held: [Mob]) -> int { return 0 }";
    let enforced = |errors: &[String]| {
        ["field `mobs`", "parameter `held`"].iter().all(|site| {
            errors
                .iter()
                .any(|e| e.contains(&format!("{site} cannot store identity struct `")))
        })
    };
    let undeclared =
        identity_storage_errors("<stdlib>/fixture.rock", "fn print() {}", Some("game"), user);
    assert!(enforced(&undeclared), "{undeclared:?}");
    let unpackaged = identity_storage_errors("<stdlib>/fixture.rock", "fn print() {}", None, user);
    assert!(unpackaged.is_empty(), "{unpackaged:?}");

    let declared = |identity_storage: IdentityStorage| {
        let mut module = parse_rock_with_file(user, "main.rock").expect("parse user module");
        module.package = Some(tag(1, "migrating", identity_storage));
        let ws = semantic_pass_into(Default::default(), &mut [module], &Default::default())
            .expect("semantic pass");
        diagnostic_messages(&ws)
    };
    let exempt = declared(IdentityStorage::Exempt);
    assert!(exempt.is_empty(), "{exempt:?}");
    let declared = declared(IdentityStorage::Enforced);
    assert!(enforced(&declared), "{declared:?}");
}

#[test]
fn identity_storage_module_exemption_follows_the_module_path() {
    use bone_semantic::identity_storage::identity_storage_exempt;

    let files = [
        ("nav.rock", "pub mod nav;", "NavMob"),
        ("nav/graph.rock", "pub mod nav::graph;", "GraphMob"),
        ("navigation.rock", "pub mod navigation;", "NavigationMob"),
        ("world.rock", "pub mod world;", "WorldMob"),
        ("lib.rock", "", "RootMob"),
    ];
    let mob = |mob: &str| {
        format!("pub struct {mob} {{ hp: int }}\npub struct {mob}Room {{ mobs: [{mob}] }}\n")
    };
    let build = |game: IdentityStorage| {
        let mut modules: Vec<_> = files
            .iter()
            .map(|(file, header, name)| {
                parse_in(&format!("{header}\n{}", mob(name)), file, (1, "game"))
            })
            .collect();
        modules.push(parse_in(
            "struct RootExtraMob { hp: int }\nstruct RootExtraMobRoom { mobs: [RootExtraMob] }\n",
            "unheaded.rock",
            (1, "game"),
        ));
        modules.push(parse_in(
            &format!("pub mod world;\n{}", mob("OtherMob")),
            "other/world.rock",
            (2, "other"),
        ));
        for module in &mut modules {
            if let Some(package) = module.package.as_mut().filter(|p| p.id == 1) {
                package.identity_storage = game.clone();
            }
        }
        modules[6].package.as_mut().unwrap().identity_storage = IdentityStorage::Enforced;
        semantic_pass_with_aliases(
            &mut modules,
            &aliases(&[(1, "game", &[]), (2, "other", &[])]),
        )
        .expect("semantic pass")
    };

    let ws = build(IdentityStorage::Enforced);
    let errors = diagnostic_messages(&ws);
    for (_, _, mob) in files {
        assert!(
            errors
                .iter()
                .any(|e| e.contains("cannot store identity struct")
                    && e.contains(&format!("{mob}`;"))),
            "every module holds the same violation while `game` is enforced: {errors:?}"
        );
    }

    let ws = build(IdentityStorage::ExemptModules(vec![
        "nav".to_string(),
        "world".to_string(),
    ]));
    let exemption_of =
        |file: &str| identity_storage_exempt(&ws, ws.module(module_by_file(&ws, file)));
    assert!(exemption_of("nav.rock"));
    assert!(exemption_of("nav/graph.rock"));
    assert!(exemption_of("world.rock"));
    assert!(!exemption_of("navigation.rock"));
    assert!(!exemption_of("other/world.rock"));
    assert!(!exemption_of("lib.rock"));
    assert!(ws.fragment_scopes.contains_key(&ustr("unheaded.rock")));
    assert!(!exemption_of("unheaded.rock"));
    let errors = diagnostic_messages(&ws);
    assert!(
        !errors
            .iter()
            .any(|e| e.contains("NavMob`;") || e.contains("GraphMob`;")),
        "exempt modules get no diagnostics: {errors:?}"
    );
    assert!(
        errors.iter().any(|e| e.contains("NavigationMob`;")),
        "{errors:?}"
    );
}

fn annotation_resolution_errors(sources: &[(&str, &str)]) -> Vec<String> {
    let mut modules: Vec<_> = sources
        .iter()
        .map(|(file, source)| parse_in(source, file, (1, "sample")))
        .collect();
    match semantic_pass_with_aliases(&mut modules, &aliases(&[(1, "sample", &[])])) {
        Err(error) => vec![error.message],
        Ok(ws) => diagnostic_messages(&ws),
    }
}

#[test]
fn nested_decorator_spelled_consume_is_not_the_builtin() {
    let errors = annotation_resolution_errors(&[(
        "main.rock",
        "mod inner; deco init consume {} @consume struct Owned { item: int }",
    )]);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("annotation `@consume` is not allowed on a struct")),
        "{errors:?}"
    );
}

#[test]
fn annotations_resolve_like_other_names() {
    let ambiguous = annotation_resolution_errors(&[
        ("a.rock", "mod a; pub deco init test {}"),
        ("b.rock", "mod b; pub deco init test {}"),
        ("main.rock", "use a::*; use b::*; @test fn probe() {}"),
    ]);
    assert!(
        ambiguous
            .iter()
            .any(|error| error.contains("test") && error.contains("ambiguous")),
        "an ambiguous annotation does not select the builtin: {ambiguous:?}"
    );

    for (name, diagnostic) in [
        ("missing", "did not resolve"),
        ("changed", "not a single definition"),
    ] {
        let source = format!(
            "use a::*; use b::*; deco init wire {{}} @wire(onchange=[{name}]) fn probe() {{}}"
        );
        let errors = annotation_resolution_errors(&[
            ("a.rock", "mod a; pub fn changed() {}"),
            ("b.rock", "mod b; pub fn changed() {}"),
            ("main.rock", &source),
        ]);
        assert!(
            errors
                .iter()
                .any(|error| error.contains(name) && error.contains(diagnostic)),
            "{name}: {errors:?}"
        );
    }

    let custom = annotation_resolution_errors(&[(
        "main.rock",
        "deco init frame_ref_source {}
        value Position { x: int }
        @frame_ref_source
        fn same(position: ref Position) -> ref Position { return position }
        fn main() {
            let positions = [Position { x: 1 }]
            let position: ref Position = same(positions[0])
            position.x = 3
        }",
    )]);
    assert!(
        custom
            .iter()
            .any(|error| error.contains("relocatable Vec or Map storage")),
        "{custom:?}"
    );

    for valid in [
        &[
            ("a.rock", "mod a; pub deco init test {}"),
            ("main.rock", "use a::*; @test fn probe() {}"),
        ][..],
        &[(
            "main.rock",
            "deco init wire {} fn changed() {} @wire(onchange=[changed]) fn probe() {}",
        )],
    ] {
        let errors = annotation_resolution_errors(valid);
        assert!(errors.is_empty(), "{valid:?}: {errors:?}");
    }
}

#[test]
fn glob_imported_names_claim_an_annotation_only_as_decorators() {
    let field = "use a::*; use b::*; fn probe(@size(4) values: [int]) {}";
    let errors = |a: &str, b: &str| {
        annotation_resolution_errors(&[
            ("a.rock", &format!("mod a; {a}")),
            ("b.rock", &format!("mod b; {b}")),
            ("main.rock", field),
        ])
    };
    for definition in ["pub fn size() {}", "pub global size: int = 1"] {
        assert_eq!(errors(definition, definition), [""; 0], "{definition}");
    }
    let ambiguous = errors("pub deco size() {}", "pub deco size() {}");
    assert!(
        ambiguous.iter().any(|error| error.contains("ambiguous")
            && error.contains("sample::a::size")
            && error.contains("sample::b::size")),
        "{ambiguous:?}"
    );
    let mixed = errors("pub deco size() {}", "pub fn size() {}");
    assert!(
        mixed
            .iter()
            .any(|error| error.contains("it applies to a fn") && !error.contains("ambiguous")),
        "{mixed:?}"
    );
}
