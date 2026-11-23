# The Rock language guide

Rock aims to retain Lua's dynamic feel while providing static typing, structured
data, pattern matching and generics. You write `.rock` files, and the
`rock` compiler checks them and runs them on LuaJIT; the same compiler
also lowers `@shader` functions to GLSL. This guide describes
what the compiler accepts today, feature by feature, with runnable
examples.

Every example in this guide is a file under
[`examples/guide/`](../examples/guide/), kept in sync by an automated
test: each fenced program below is one of those files byte for byte,
each `NN_topic.rock` prints its `NN_topic.out` twin when run, and the
shader examples compile to GLSL. The commands in this guide
run from that directory of a clone:

```sh
git clone https://github.com/darthdeus/rock-lang.git
cd rock-lang/examples/guide
rock run 01_hello.rock
```

`rock run` and `rock test` execute programs with LuaJIT, so `luajit`
must be on `PATH`. See the README for install instructions.

Advanced facilities are covered in the [language reference](language-reference.md)
and [coroutine reference](coroutines.md). Contributors can start with
[compiler structure](compiler.md) and [testing](testing.md).

## Your first program

A program is a file with a `main` function. `print` writes a line.

```rock
// Your first Rock program. Save it as hello.rock and run it:
//
//     rock run hello.rock
//
fn main() {
    print("hello from Rock")
    let answer = 6 * 7
    print(answer)
    print("6 times 7 is {answer}")
}
```

```sh
rock run hello.rock
```

prints

```text
hello from Rock
42
6 times 7 is 42
```

The commands you will use most:

- `rock run file.rock` (or a project directory) compiles and runs.
- `rock test file.rock` runs the `@test` functions (see Tests).
- `rock check file.rock` only typechecks, without running anything.
- `rock fmt file.rock` formats in place; `--check` only reports.
- `rock compile file.rock out.lua` emits the generated Lua instead of
  running it.

Diagnostics point at the source with a span, for example:

```text
[E00] Error: type mismatch: expected `uint`, found `int`
```

## Values and types

The primitive types are `int`, `uint`, `float`, `bool` and `string`.
`any` is the escape hatch for values whose type the compiler cannot
know. `let` introduces a local, with an optional `: Type` annotation,
and locals are reassignable; there is no `mut` keyword.

```rock
// Numbers, bools and strings. Rock's primitives are
// int, uint, float, bool and string (plus any, the escape hatch).
fn main() {
    print(7 / 2) // `/` always divides: 3.5, even for ints
    print(7 ./ 2) // `./` is integer division: 3
    print(7 % 3) // remainder: 1
    print(0xFF) // hex literals: 255 (no digit separators)
    print(3 as float) // convert with `as`: 3
    print(3.9 as int) // float to int truncates: 3
    print(-3.9 as int) // floor toward negative infinity: -4
    print(true && !false) // bools: true
    print(1 < 2) // comparisons: true
    print("ab" + "c") // `+` concatenates strings: abc
    let n: uint = 7
    print(n + (1 as uint)) // uint arithmetic: 8 (no mixed int/uint ops)
}
```

Three things surprise newcomers:

- `/` always divides: `7 / 2` is `3.5`, even for two ints. `./` is
  integer division, `%` the remainder.
- Number literals have no digit separators (`1000`, not `1_000`),
  and `int` and `uint` do not combine without `as`.
- `as` converts between numeric types only.

## Structs

A `struct` groups named fields. Struct values are shared handles:
mutating one through any variable is visible through the others.
Methods are functions whose name starts with `Type.` and whose first
parameter is the receiver, written without a type.

```rock
// Structs group named fields. A struct value is a shared handle:
// mutating it through one variable is visible through the others.
struct Point {
    x: float,
    y: float,
}

// A method names its receiver first, without a type.
fn Point.length(p) -> float {
    return (p.x * p.x + p.y * p.y).sqrt()
}

fn main() {
    let p = Point { x: 3.0, y: 4.0 }
    print(p.length()) // 5
    let q = Point { ..p, x: 1.0 } // spread copies the rest: q.y is 4
    print(q.y)
    p.x = 10.0 // fields are mutable in place
    print(p.x)
}
```

`..p` spreads the remaining fields of another value of the same type;
it comes before any overriding fields.

## Value types

A `value` looks like a struct but copies on assignment and on calls,
so a function cannot mutate its caller's copy. Assigning a field of a
value local or parameter replaces that binding's whole value, nested
fields included; no other copy sees the change. A call result is a
temporary, so its fields cannot be assigned. The math types (`Vec2`,
`Vec3`, `Vec4`, `Mat4`) are value types.

A value is stored inline, so it cannot contain itself by value, even
as `T?` or through other values: `value Node { next: Node? }` is a
compile error. Recurse through a reference instead, such as a Vec, a
Map, an enum or an `Entity<T>` handle.

```rock
// `value` types look like structs but copy on assignment and on calls,
// so a field write changes only that one copy.
struct Box {
    n: int,
}

value Dot {
    n: int,
}

fn bump_box(b: Box) {
    b.n = b.n + 1
}

fn bump_dot(d: Dot) -> Dot {
    d.n = d.n + 1 // writes this call's own copy
    return d
}

fn main() {
    let b = Box { n: 0 }
    bump_box(b)
    print(b.n) // 1: the struct is shared, the caller's copy changed
    let d = Dot { n: 0 }
    let d2 = bump_dot(d)
    print(d.n) // 0: the value was copied, the caller's copy is untouched
    print(d2.n) // 1
    let e = d
    e.n = 99 // replaces `e`'s whole value; `d` is another copy
    print(d.n) // 0
    print(e.n) // 99
}
```

A struct has identity, so it lives in one place: an `Arena<T>` row or
a global. Fields, `Vec` and `Map` elements, enum payloads, escaping
closure captures and coroutine state hold a value or an `Entity<T>`
handle instead; a struct local or parameter is a borrow. The compiler
reports `cannot store identity struct` for anything else. A package
that has not migrated to this rule yet declares it in its
`stick.toml`, and its violations are not reported:

```toml
[language]
identity_storage = "exempt"                    # the whole package
# identity_storage = { exempt_modules = ["nav"] } # `nav` and modules below it
```

Absent, or `"enforced"`, the rule applies. The standard library and
files outside any package are exempt.

## Newtypes

A `newtype` wraps one value in a distinct type, so the compiler stops
you passing a `UserId` where a plain `int` was meant, or the reverse.
Construct it like a function call, unwrap it with `.0`.

```rock
// A newtype wraps one value in a distinct type, so the compiler
// rejects mixing up two ints (or strings) that mean different things.
newtype UserId(int)

fn main() {
    let id = UserId(7)
    print(id) // 7: it prints like the wrapped value
    print(id.0) // unwrap with `.0`: 7
}
```

## Enums and match

An `enum` lists the variants a value can be. Variants are empty, hold
tuple data, or hold named fields. `match` dispatches on them; the
compiler rejects a match that misses a variant, so add a `_` arm or
cover everything. Arms can carry an `if` guard.

```rock
// Enums list the variants a value can be; `match` dispatches on them.
// Variants can carry data as a tuple or as named fields.
enum Shape {
    Point,
    Circle(float),
    Rect { w: float, h: float },
}

fn area(s: Shape) -> float {
    return match s {
        Shape::Point => 0.0,
        Shape::Circle(r) => 3.14159 * r * r,
        Shape::Rect { w, h } => w * h,
    }
}

fn size_name(n: int) -> string {
    return match n {
        0 => "none",
        x if x > 100 => "big", // arms can carry an `if` guard
        _ => "small", // `_` matches anything left over
    }
}

fn main() {
    print(area(Shape::Circle(1.0))) // 3.14159
    print(area(Shape::Rect { w: 2.0, h: 3.0 })) // 6
    print(size_name(0)) // none
    print(size_name(150)) // big
    print(size_name(5)) // small
}
```

`match` is also an expression: `let s = match c { ... }` works wherever
a value is expected. There is no `switch`.

## Optionals

Only `T?` can hold `null`; a plain `T` never is. `?.` evaluates to
null instead of crashing when the receiver is null, `??` picks the
right side when the left is null, and `?.let` runs a block only for a
non-null value. `unwrap()` and `expect("why")` abort on null. Like `??`
and `?.`, they apply only to a value that can be null: on a plain `T`, or
a `T?` already checked against null, the compiler rejects them as
redundant.

```rock
// A `T?` holds a `T` or `null`. `?.` stops at null instead of crashing,
// and `??` picks a fallback when the left side is null.
struct Named {
    name: string?,
}

fn greet(n: Named?) -> string {
    return "hi " + (n?.name ?? "stranger")
}

fn first(items: [int]) -> int? {
    if items.len() == 0 {
        return null
    }
    return items[0]
}

fn display_name(n: Named?) -> string {
    // `?.let` runs the block only when the value is not null:
    return n?.let { n.name ?? "?" } ?? "nobody"
}

fn main() {
    print(greet(null)) // hi stranger
    print(greet(Named { name: "ada" })) // hi ada
    print(greet(Named { name: null })) // hi stranger
    let xs = [10, 20]
    print(first(xs) ?? -1) // 10
    print(first([]) ?? -1) // -1
    print(display_name(Named { name: "jo" })) // jo
    print(display_name(null)) // nobody
}
```

## Tuples

Tuples group a fixed mix of values of different types, handy for
returning more than one thing. Destructure with `let (a, b) = t`
(`_` skips an element) or index from zero: `t.0`, `t.1`.

```rock
// Tuples group a fixed mix of values. Destructure them with `let`,
// or index from zero with `.0`, `.1` and so on.
fn divmod(a: int, b: int) -> (int, int) {
    return (a ./ b, a % b)
}

fn main() {
    let t = (1, "two", 3.0)
    let (a, b, _) = t
    print(a) // 1
    print(b) // two
    print(t.0) // 1
    print(t.2) // 3
    let (q, r) = divmod(7, 3)
    print(q) // 2
    print(r) // 1
    let nested = ((1, 2), (3, 4))
    print(nested.1.0) // 3
}
```

## Destructuring and let else

`let` and `for` take the same patterns `match` does, as long as the
pattern cannot fail: struct fields, tuples, newtypes, and their
nestings. A pattern that can fail -- an enum variant, or any `let` over
a nullable value -- takes an `else` block, which runs on a miss and
must diverge (`return`, `break`, or `continue`). The bindings live on
in the enclosing scope, unlike `if let`.

```rock
// `let` and `for` accept patterns, not just names: destructure structs,
// tuples and newtypes in place. `let ... else` handles the refutable cases.
struct Point {
    x: int,
    y: int,
}

enum Shape {
    Circle(int),
    Square(int, int),
}

fn radius(s: Shape) -> int {
    // A pattern that can fail needs an `else` block, which runs on a miss
    // and must leave the function (or loop): `return`, `break`, `continue`.
    let Shape::Circle(r) = s else {
        return 0
    }
    return r // `r` stays in scope for the rest of the function
}

fn main() {
    let p = Point { x: 3, y: 4 }
    let Point { x, y } = p // fields by name; `Point { x, .. }` skips the rest
    print(x + y) // 7

    let pts = [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }]
    for Point { x, .. } in pts {
        print(x) // 1, then 3
    }

    print(radius(Shape::Circle(5))) // 5
    print(radius(Shape::Square(2, 3))) // 0

    // `let ... else` also unwraps optionals: the else path runs on null.
    let m = map_new<string, int>()
    m.insert("a", 7)
    let v = m.get("a") else {
        return
    }
    print(v) // 7
}
```

## Generics

Functions and structs take type parameters in angle brackets. Each use
is checked at its concrete types. Methods on a generic type repeat the
parameters on both sides: `fn Pair<T>.swap(p)`.

```rock
// Functions and structs take type parameters in angle brackets.
// Each use is checked (and compiled) at its concrete types.
fn first<T>(items: [T]) -> T? {
    if items.len() == 0 {
        return null
    }
    return items[0]
}

struct Pair<T> {
    left: T,
    right: T,
}

fn Pair<T>.swap(p) -> Pair<T> {
    return Pair { left: p.right, right: p.left }
}

fn main() {
    print(first([10, 20]) ?? -1) // 10
    print(first(["a", "b"]) ?? "?") // a
    let p = Pair { left: 1, right: 2 }
    print(p.swap().left) // 2
    let words = Pair { left: "x", right: "y" }
    print(words.right) // y
}
```

## Functions

Parameters can have defaults, and any argument can be passed by name
(`name: value`); positional arguments must come before named ones.
Closures are written `x => expr` or `x => { ... }`, and a function
type is written `fn(int) -> int`. Several functions may share one
name with different signatures; a call that matches two of them
equally well is rejected as ambiguous.

An explicit `null` argument overrides a default: with
`seed: int? = 42`, `f()` receives 42 and `f(null)` receives null.

```rock
// Parameters can have defaults, and callers can pass any argument by
// name. Closures use `=>`; a function type is written `fn(T) -> U`.
fn greet(name: string = "world", loud: bool = false) -> string {
    let g = "hello " + name
    if loud {
        return g.upper()
    }
    return g
}

fn apply(f: fn(int) -> int, x: int) -> int {
    return f(x)
}

fn main() {
    print(greet()) // hello world
    print(greet("ada")) // hello ada
    print(greet(name: "ada", loud: true)) // HELLO ADA
    let double = x => x * 2
    print(double(21)) // 42
    print(apply(n => n + 1, 41)) // 42
    let with_block = x => {
        let y = x * 10
        return y + 1
    }
    print(with_block(4)) // 41
}
```

## Control flow

`if` is an expression and chains with `else if`. `while` loops on a
condition, `loop` loops forever until `break`, and `for .. in` walks
ranges, vectors and other collections. `break` and `continue` work in
all three. Ranges exclude the end: `0..3` yields 0, 1, 2.

```rock
// `if` is an expression, and chains with `else if`. Loops are `while`,
// `loop` (with `break`/`continue`) and `for .. in` over ranges,
// vectors and other collections.
fn sign_name(n: int) -> string {
    if n > 0 {
        return "pos"
    } else if n < 0 {
        return "neg"
    } else {
        return "zero"
    }
}

fn main() {
    print(sign_name(-3)) // neg
    let label = if true { "yes" } else { "no" }
    print(label) // yes
    for i in 0..3 {
        print(i) // 0, 1, 2: ranges exclude the end
    }
    let total = 0
    for x in [10, 20, 30] {
        total = total + x
    }
    print(total) // 60
    let n = 0
    while n < 2 {
        n = n + 1
    }
    print(n) // 2
    let count = 0
    count += 5
    count -= 2
    print(count) // 3
    let i = 0
    loop {
        i = i + 1
        if i < 3 {
            continue
        }
        break
    }
    print(i) // 3
}
```

## Vectors

`[T]` is a growable vector, written `[]` or `[1, 2, 3]`. Indexing
starts at 0. `v[i]` aborts the program when out of bounds; `v.get(i)`
returns null instead. `enumerate` pairs each element with its index.

```rock
// `[T]` is a growable vector. Indexing starts at 0; `v[i]` aborts
// when out of bounds, while `v.get(i)` returns null instead.
fn main() {
    let xs: [int] = [1, 2, 3]
    xs.push(4)
    print(xs.len()) // 4
    print(xs[0]) // 1
    print(xs.get(99) ?? -1) // -1
    print(xs.pop() ?? -1) // 4
    xs.remove_at(0)
    print(xs[0]) // 2
    for (i, x) in enumerate(xs) {
        print("{i}:{x}") // 0:2, 1:3
    }
    let words = ["a", "b"]
    print(words.rev()[0]) // b
    print(words.is_empty()) // false
    words.clear()
    print(words.len()) // 0
}
```

Other methods include `set`, `swap_remove` and `rev`. (Despite the
name, the `Vec2`/`Vec3`/`Vec4` math types are unrelated fixed-size
value types, not vectors.)

`v.sort(key)` sorts in place, ascending by `key(element)`. A number or
string key compares with `<`. A struct or tuple key compares its fields in
order, nested values flattened, and the first unequal field decides:
`cells.sort(c => (c.row, c.col))` sorts by row, then by column. Such a key
may hold only `int`, `uint`, `float` and `string` fields; an optional or
`bool` field is a compile error. The sort is not stable, so elements with
equal keys can end up in any order.

## Maps

`Map<K, V>` is a hash map, made with `map_new()`. Like vectors, `m[k]`
aborts on a missing key while `m.get(k)` returns null. Iteration order
is unspecified; `m.keys()` returns the keys sorted.

```rock
// `Map<K, V>` is a hash map. Iteration order is unspecified,
// so this example walks the sorted keys for stable output.
fn main() {
    let m: Map<string, int> = map_new()
    m.insert("b", 2)
    m.insert("a", 1)
    print(m.len()) // 2
    print(m["a"]) // 1: `m[k]` aborts on a missing key
    print(m.get("zzz") ?? -1) // -1: `.get` returns null instead
    print(m.contains("b")) // true
    for k in m.keys() {
        print("{k}={m[k]}") // a=1, b=2
    }
    print(m.remove("a") ?? -1) // 1
    print(m.len()) // 1
    m.clear()
    print(m.is_empty()) // true
}
```

## Strings

String literals interpolate `{expr}`; write `{{` for a literal brace.
Offsets and lengths count bytes (UTF-16 units on the JS backend);
`chars()` yields Unicode code points on both. The method list below
is the core; `upper`, `lower`, `repeat`, `starts_with`, `ends_with`,
`replace`, `pad_left`, `pad_right`, `lines` and `parse_float` work the
same way. `upper` and `lower` map ASCII letters only. `substring(a, b)`
is Lua `string.sub(a + 1, b)` on both backends, so a negative bound
counts from the end (`"abc".substring(0, -1)` is `"abc"`); pass
non-negative bounds unless you want that.

`print` renders a float with at most 14 significant digits and no
trailing zeros: `3.0` prints as `3`, `1e16` as `1e+16`, and non-finite
values as `inf`, `-inf` and `nan`. `{x}` with no format spec keeps at
most 4 decimals (`{3.14159}` is `3.1416`) while the magnitude is in
`[1e-4, 1e6)`; outside it `{x}` prints like `print` (14 significant
digits, so `{-7.25e16}` is `-7.25e+16`). Integers never print as `-0`.
`to_string(x)` gives the
`print` text, but adds `.0` when that text is only digits and an
optional minus, so `to_string(3.0)` is `"3.0"` while `to_string(1e16)`
stays `"1e+16"`.

```rock
// Strings interpolate `{expr}`; write `{{` for a literal brace.
// Offsets count bytes. The stdlib adds split, trim and friends.
fn main() {
    let name = "rock"
    print("hello {name}, {1 + 2}") // hello rock, 3
    print("literal {{braces}}") // literal {braces}
    print("abc".len()) // 3
    print("abc".upper()) // ABC
    print("  hi  ".trim()) // hi
    print("a,b,c".split(",").join(";")) // a;b;c
    print("hello".contains("ell")) // true
    print("hello".find("l")) // 2: first match, or -1
    print("hello".substring(1, 4)) // ell
    print("42".parse_int() ?? -1) // 42
    print("nope".parse_int() ?? -1) // -1
    for cp in "hey".chars() {
        print(cp) // 104, 101, 121: Unicode code points
    }
}
```

## JSON

`json_decode` parses into the `JsonValue` enum and fails softly with
null; `json_encode` renders the compact form. The `json_as_*` helpers
and `json_field` pull typed data out. Both `json_encode` and
`json_encode_pretty` write floating-point negative zero as `-0.0` so decoding
preserves its sign. A bare `-0` token takes the integer path.

```rock
// The stdlib decodes JSON into the JsonValue enum and encodes it back.
// Decoding fails softly with null; field access is explicit.
fn main() {
    let v = json_decode("{{\"a\": 1, \"b\": [true, null]}}")
    print(json_encode(v ?? json_null())) // {"a":1,"b":[true,null]}
    let obj = json_as_object(v ?? json_null()) ?? map_new()
    print(json_as_int(obj.get("a") ?? json_null()) ?? -1) // 1
    print(json_decode("{{oops}}") == null) // true
    let built = json_object(map_new())
    print(json_encode(built)) // {}
}
```

## Release packages

`rock package` selects project files from `[release]` in `stick.toml`. If
`assets` is omitted, it defaults to the `assets` root. The worktree bank
`assets/visual_bank.jsonl` is included implicitly when present;
`visual_banks` adds the listed bank files. Entries must be canonical
`assets/...jsonl` paths, and each bank file must exist in the worktree; overlay
banks are not accepted, including through `overlay_assets`. Globs, traversal,
and noncanonical paths are rejected. Listing the main bank explicitly makes
its absence an error. Repeated bank entries are harmless.

```toml
[release]
assets = ["assets/audio", "saves"]
visual_banks = ["assets/ui.jsonl", "assets/world.jsonl"]
overlay_assets = ["assets/ui/cursor.png"]
```

Each declared bank is validated, and its referenced sheet files form the
packaged dependency closure. Sheet files resolve from configured overlays in
their listed order, then from the worktree. A generation split between roots
or corrupt data in a higher-priority root fails validation. Shared dependencies
are packaged once, but each bank reference is validated. `rock asset-check` and
`rock package` perform the same validation before creating or changing package
output.

An `assets` root may already select files in a bank's dependency closure. If
`overlay_assets` repeats one of those sheet files, both commands warn
that the bank already supplies it; remove the redundant entry. Other listed
overlay files, including loose files, are still packaged. A nonempty, valid
JSONL bank selected by `assets` but not listed in `visual_banks` is packaged
with a warning, without automatically selecting its dependencies; those
dependencies must already be selected separately.

## Modules and use

A project is a directory with a `stick.toml`. Its `.rock` files contribute
to scoped modules. A standalone file has an implicit one-file package:

```toml
[package]
name = "greeter"

[language]
modules = "v1"
```

A `mod` header assigns a file to a logical module;
without a header it belongs to the package root. Multiple files can
contribute to the same module. Items are private to their defining module
and its descendants unless exported. Here `super::` addresses the parent
module, so `main` reaches its sibling through `super::helpers::shout`:

```rock
mod main;

use super::helpers::shout;

fn main() {
    print(shout("hello"))
}
```

```rock
mod helpers;

pub fn shout(s: string) -> string {
    return s.upper() + "!"
}

fn whisper(s: string) -> string {
    return s.lower()
}
```

The files live in `16_modules/`; `rock run 16_modules` (or `rock run .`
inside it) prints `HELLO!`. See the [modules reference](modules.md) for
imports, visibility, and package dependencies.

## Tests

`@test` marks a test; `rock test` runs them all. A test fails when it
aborts: a failed `assert`, an error, a crash. With `output = "..."`,
the test additionally fails unless what it prints matches exactly.

```rock
// `@test` marks a test; `rock test` runs them all. A test fails when it
// aborts (a failed assert, an error, a crash). With `output = "..."`,
// the test also fails unless its printed output matches exactly.
//
//     rock test 17_tests.rock
//
fn add(a: int, b: int) -> int {
    return a + b
}

@test
fn adds() {
    assert(add(2, 3) == 5)
}

@test(output = "7")
fn prints_sum() {
    print(add(3, 4))
}

fn main() {
    print(add(1, 2))
}
```

```sh
rock test 17_tests.rock
```

reports `rock test: 2/2 @tests passed`. Plain `rock run` still runs
`main`, so one file can carry both the program and its tests.

For testing compiler changes from a checkout, filtering individual cases, and
asserting expected compiler errors, see [Testing compiler changes](testing.md).

## Shaders

A `@shader(vertex)` entry runs per vertex, a `@shader(fragment)` entry
per pixel. Entries take `@attr` attributes, `@uniform` uniforms and
plain parameters, and return varyings (vertex) or a color (fragment).
Entries are compiled for the GPU, not the CPU, and the GPU-only
helpers (`texture()`, `frag_coord()`, `discard()` and the like) abort
when called on the CPU. Attribute, uniform, varying and sampler names
share one namespace per program, so keep them distinct.

A vertex+fragment pipeline:

```rock
// A vertex+fragment pipeline. The vertex entry reads per-vertex
// attributes and uniforms; the fragment entry shades each pixel.
// Compile it to GLSL (never run it on the CPU):
//
//     rock shader 18_shader_pipeline.rock tint_vert tint_frag
//
value Vertex {
    pos: Vec2,
    st: Vec2,
}

value Camera {
    projection: Mat4,
    view: Mat4,
    model: Mat4,
}

value Varying {
    position: Vec4,
    uv: Vec2,
}

@shader(vertex)
fn tint_vert(@attr a: Vertex, @uniform u: Camera) -> Varying {
    let clip = u.projection * u.view * u.model * Vec4 { x: a.pos.x, y: a.pos.y, z: 0.0, w: 1.0 }
    return Varying { position: clip, uv: a.st }
}

@shader(fragment)
fn tint_frag(vary: Varying) -> Vec4 {
    return Vec4 { x: 1.0, y: 0.0, z: 1.0, w: 1.0 }
}
```

A fullscreen fragment entry is just a function of the pixel coordinate:

```rock
// A fullscreen fragment entry: no vertex shader, just a function of
// the pixel coordinate. Compile it the same way:
//
//     rock shader 19_shader_fullscreen.rock tint
//
@shader(fragment)
fn tint(tc: Vec2) -> Vec4 {
    return v(tc.x, tc.y, 0.5, 1.0)
}
```

`rock shader` compiles entries to GLSL; it prints to stdout, or writes
`--output file.glsl`. One root compiles a fullscreen fragment entry,
two roots compile a vertex+fragment pipeline, anything else compiles a
plain function library. The GLSL profile is `330core`.

## Backends

The default backend compiles through Lua and runs on LuaJIT.
`rock run --backend js file.rock` (or `rock test --backend js`) compiles
to JavaScript instead and runs it with `node`, which must be on `PATH`.
`rock check` only typechecks.

`rock compile` emits the Lua without running it; `rock shader`
emits GLSL as described above.

## What Rock does not have

A short list for readers coming from other languages:

- No classes, inheritance or interfaces; structs with methods, value
  types and enums cover that ground.
- No exceptions; there is no `try`/`catch`, failures abort.
- No `switch`; `match` on enums (and on ints and strings) covers it.
- No null for non-optional types; only `T?` can be null.
- No integer `/`; it always divides, `./` is the integer operator.
- No digit separators in number literals and no mixed `int`/`uint`
  arithmetic; convert explicitly with `as`.
