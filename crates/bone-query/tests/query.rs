//! Definition and reference ranges through the LSP workspace facade.

use crate::completion::cursor;
use crate::fixture::{PATH, offset_of, workspace};
use bone_query::FileUriExt;

/// Byte offset of the `occurrence`-th (1-based) appearance of `needle` in `src`.
fn nth_offset(src: &str, needle: &str, occurrence: usize) -> usize {
    src.match_indices(needle)
        .nth(occurrence - 1)
        .unwrap_or_else(|| panic!("occurrence {occurrence} of {needle:?} not found"))
        .0
}

#[test]
fn definition_cases() {
    // The definition span must contain the declaration name; it may be the
    // bare name or the whole item.
    // (case, source, use needle, offset into it, declaration needle, offset into it)
    let cases = [
        (
            "local",
            "fn f() -> int {\n    let x = 1\n    return x\n}",
            "return x",
            7,
            "let x",
            4,
        ),
        (
            "parameter",
            "fn f(n: int) -> int {\n    return n\n}",
            "return n",
            7,
            "f(n",
            2,
        ),
        (
            "function call",
            "fn helper() -> int {\n    1\n}\nfn main() {\n    let r = helper()\n}",
            "= helper()",
            2,
            "fn helper",
            3,
        ),
        (
            "struct literal",
            "struct Point { x: int }\nfn f() -> Point {\n    Point { x: 1 }\n}",
            "Point { x: 1",
            0,
            "struct Point",
            7,
        ),
        (
            "enum qualifier",
            "enum Color { Red, Green }\nfn f() -> Color {\n    Color::Red\n}",
            "Color::Red",
            0,
            "enum Color",
            5,
        ),
        (
            "global",
            "global G: int = 5\nfn f() -> int {\n    return G\n}",
            "return G",
            7,
            "global G",
            7,
        ),
        // Like a `mob.attack_co` access, a FieldRef key path names the field.
        (
            "field ref",
            "struct Mob { health: int, attack_co: int? }\nfn kill_co<T>(o: T, f: FieldRef<T, int?>) {}\nfn r(mob: Mob) {\n    kill_co(mob, .attack_co)\n}\n",
            ".attack_co",
            1,
            "attack_co: int?",
            0,
        ),
    ];
    for (case, src, use_needle, use_into, decl_needle, decl_into) in cases {
        let at = crate::fixture::pos(src, use_needle, use_into);
        let decl_off = nth_offset(src, decl_needle, 1) + decl_into;
        let def = workspace(src)
            .definition(PATH, at)
            .unwrap_or_else(|| panic!("{case}: no definition at {at:?}"));
        assert_eq!(def.uri.to_file_path().unwrap(), PATH);
        let (s, e) = (
            offset_of(src, def.range.start),
            offset_of(src, def.range.end),
        );
        assert!(
            s <= decl_off && decl_off < e,
            "{case}: {s}..{e} misses {decl_off}"
        );
        if case == "function call" {
            assert_eq!(e - s, "helper".len(), "{case}: the bare name, not the item");
        }
    }
}

#[test]
fn definition_starts() {
    // (case, source with a `<|>` cursor and a `<@>` at the definition start)
    let cases: &[(&str, &str)] = &[
        (
            "closure captured var",
            r"fn main() {
    let <@>base = 10
    let f = (x: int) => x + <|>base
}",
        ),
        (
            "enum qualified variant",
            r"enum Direction {
    Up,
    <@>Down,
}
fn main() {
    let d = Direction::<|>Down
}",
        ),
        (
            "enum record variant",
            r"enum Shape {
    <@>Circle { radius: float },
    Rect { w: float, h: float },
}
fn main() {
    let s = Shape::<|>Circle { radius: 5.0 }
}",
        ),
        (
            "enum simple variant",
            r"enum Color {
    <@>Red,
    Green,
    Blue,
}
fn main() {
    let c = Color::<|>Red
}",
        ),
        (
            "for loop iterable",
            r"fn main() {
    let <@>items = [1, 2, 3]
    for x in <|>items {
        print(x)
    }
}",
        ),
        (
            "function call",
            r"fn <@>double(x: int) -> int {
    return x * 2
}
fn main() {
    let x = 5
    let y = <|>double(x)
}",
        ),
        (
            "generic struct literal",
            r"struct <@>Box<T> {
    item: T
}
fn main() {
    let b = <|>Box { item: 42 }
}",
        ),
        (
            "global variable assignment",
            r"global <@>counter: int = 0
fn main() {
    <|>counter = 42
}",
        ),
        (
            "mixed shorthand x",
            r"struct Point {
    x: int
    y: int
}
fn main() {
    let <@>x = 100
    let p = Point { <|>x, y: 200 }
}",
        ),
        (
            "nested field access",
            r"struct Inner {
    <@>value: int
}
struct Outer {
    inner: Inner
}
fn main() {
    let o = Outer { inner: Inner { value: 42 } }
    let v = o.inner.<|>value
}",
        ),
        (
            "record variant pattern field name",
            r"enum Opt {
    Some { <@>value: int },
    None,
}
fn main() {
    let o = Opt::Some { value: 42 }
    match o {
        Opt::Some { <|>value: v } => { print(v) },
        Opt::None => {},
    }
}",
        ),
        (
            "shorthand x value",
            r"struct Point {
    x: int
    y: int
}
fn main() {
    let <@>x = 100
    let y = 200
    let p = Point { <|>x, y }
}",
        ),
        (
            "shorthand y value",
            r"struct Point {
    x: int
    y: int
}
fn main() {
    let x = 100
    let <@>y = 200
    let p = Point { x, <|>y }
}",
        ),
        (
            "struct field in literal",
            r"struct Point {
    <@>x: float,
    y: float,
}
fn main() {
    let p = Point { <|>x: 1.0, y: 2.0 }
}",
        ),
        (
            "struct literal name",
            r#"struct <@>Player {
    name: string
    health: int
}
fn main() {
    let player = <|>Player { name: "Hero", health: 100 }
}"#,
        ),
        (
            "struct literal name assignment",
            r#"struct <@>Enemy {
    kind: string
}
fn main() {
    let e: Enemy = <|>Enemy { kind: "goblin" }
}"#,
        ),
        (
            "struct pattern field name",
            r"struct Point { <@>x: int, y: int }
fn main() {
    let p = Point { x: 1, y: 2 }
    match p {
        Point { <|>x: a, y: b } => { print(a + b) },
    }
}",
        ),
        (
            "function",
            r"fn <@>calculate(x: int, y: int) -> int {
    return x + y
}
fn main() {
    let result = <|>calculate(5, 3)
}",
        ),
        (
            "local variable",
            r"fn calculate() {
    let <@>base_value = 10
    let multiplier = 3
    let result = <|>base_value * multiplier
    print(result)
}",
        ),
        (
            "macro invocation name",
            r"macro <@>twice(x: expr) {
    let _ = x + x
}
fn main() {
    tw<|>ice(41)
}",
        ),
        (
            "parameter",
            r"fn process(<@>data: string, count: int) -> string {
    let result = <|>data + count.to_string()
    return result
}",
        ),
        (
            "struct field",
            r"struct Point {
    <@>x: float,
    y: float,
}
fn main() {
    let p = Point { x: 1.0, y: 2.0 }
    let coordinate = p.<|>x
}",
        ),
        (
            "struct type",
            r"struct <@>Vector {
    x: float,
    y: float,
}
fn create_vector() -> <|>Vector {
    return Vector { x: 0.0, y: 0.0 }
}",
        ),
        (
            "variable",
            r"fn main() {
    let <@>number = 42
    let doubled = <|>number * 2
    print(doubled)
}",
        ),
    ];
    for (case, marked) in cases {
        let (src, at) = cursor(&marked.replacen("<@>", "", 1));
        let (_, want) = cursor(&marked.replacen("<|>", "", 1).replacen("<@>", "<|>", 1));
        let def = workspace(&src)
            .definition(PATH, at)
            .unwrap_or_else(|| panic!("{case}: no definition"));
        assert_eq!(def.uri.to_file_path().unwrap(), PATH, "{case}");
        assert_eq!(def.range.start, want, "{case}");
    }
}

/// Builtin types resolve to their stdlib declaration; primitives and keywords
/// resolve to nothing.
#[test]
fn builtin_definitions() {
    // (case, source with a `<|>` cursor, whether a definition exists)
    let cases: &[(&str, &str, bool)] = &[
        (
            "builtin coroutine jumps to stdlib resume",
            r"fn use_coroutine(c: <|>Coroutine<int>) -> int? {
    return resume(c)
}",
            true,
        ),
        (
            "builtin coroutine jumps to stdlib resume landmark",
            r"co fn producer() -> <|>Coroutine<int> {
    yield 42
}",
            true,
        ),
        (
            "builtin map jumps to stdlib map new",
            r"fn make() -> <|>Map<string, int> {
    return map_new()
}",
            true,
        ),
        (
            "builtin native buffer jumps to stdlib",
            r"fn make() -> <|>NativeBuffer<int> {
    return native_buffer_int(10)
}",
            true,
        ),
        (
            "builtin native buffer u8 jumps to stdlib",
            r"fn make() -> <|>NativeBufferU8 {
    return native_buffer_u8(10)
}",
            true,
        ),
        (
            "builtin primitive int returns nothing",
            r"fn make() -> <|>int {
    return 42
}",
            false,
        ),
        (
            "builtin vec jumps to stdlib vec struct",
            r"fn make() -> <|>Vec<int> {
    return vec_new()
}",
            true,
        ),
        (
            "keyword yield returns empty",
            r"co fn producer() -> Coroutine<int> {
    <|>yield 42
}",
            false,
        ),
        (
            "primitive type returns empty",
            r"fn make() -> Vec<<|>int> {
    return vec_new()
}",
            false,
        ),
    ];
    for (case, marked, exists) in cases {
        let (src, at) = cursor(marked);
        assert_eq!(
            workspace(&src).definition(PATH, at).is_some(),
            *exists,
            "{case}"
        );
    }
}

#[test]
fn direct_enum_path_definition() {
    use bone_semantic::prelude::*;

    let src = "enum Color { Red, Green }\nfn f() -> Color { Color::Red }";
    let ast = skull_parser::parse_module_clean(src, Ustr::from(PATH)).unwrap();
    let ws = semantic_pass(&mut [ast]).unwrap();
    let mid = ws.source_modules().next().unwrap().id;
    let span = bone_query::query::definition_at(
        &ws,
        mid,
        ws.inference(mid).unwrap(),
        src.find("Color::Red").unwrap(),
    )
    .expect("enum qualifier resolves through its recorded path segment");
    let (start, end) = span.offset_range();
    assert!(start <= 5 && 5 < end, "enum declaration: {start}..{end}");
}

/// Corpus fixtures pin reference starts; keep the query-level span-width
/// invariant here because the corpus currently checks starts only.
#[test]
fn reference_ranges_cover_only_names() {
    // (case, source, name, cursor occurrence)
    let cases: &[(&str, &str, &str, usize)] = &[
        (
            "local",
            "fn f() -> int {\n    let count = 1\n    let total = count + count\n    total\n}",
            "count",
            2,
        ),
        (
            "parameter",
            "fn f(num: int) -> int {\n    num + num\n}",
            "num",
            2,
        ),
        (
            "function from a call",
            "fn helper() -> int { 1 }\nfn a() -> int { helper() }\nfn b() -> int {\n    let x = helper()\n    helper()\n}",
            "helper",
            2,
        ),
        (
            "function from its declaration",
            "fn helper() -> int { 1 }\nfn a() -> int { helper() }",
            "helper",
            1,
        ),
        // The UFCS method name is not at the call span's start.
        (
            "UFCS call",
            "fn inc(x: int) -> int { x + 1 }\nfn main() {\n    let a = inc(5)\n    let b = 5.inc()\n}",
            "inc",
            3,
        ),
        // A fn-typed param called as `g()` has no separate Var node.
        (
            "local called as a function",
            "fn apply(g: fn() -> int) -> int {\n    g() + g()\n}",
            "g",
            2,
        ),
    ];
    for (case, src, name, cursor) in cases {
        let cursor = crate::fixture::position_of_offset(src, nth_offset(src, name, *cursor));
        let refs = workspace(src).references(PATH, cursor);
        assert!(!refs.is_empty(), "{case}: no references");
        for loc in refs {
            assert_eq!(loc.uri.to_file_path().unwrap(), PATH);
            let (start, end) = (
                offset_of(src, loc.range.start),
                offset_of(src, loc.range.end),
            );
            assert_eq!(end - start, name.len(), "{case}: {start}..{end}");
        }
    }
}
