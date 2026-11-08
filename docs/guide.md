# The Rock language guide

Rock is a statically typed language. You write `.rock` files, and the
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
so a function cannot mutate its caller's copy. Fields of a value held
in a local cannot be assigned in place; replace the whole value. The
math types (`Vec2`, `Vec3`, `Vec4`, `Mat4`) are value types.

```rock
// `value` types look like structs but copy on assignment and on calls,
// and their fields cannot be mutated in place: replace them wholesale.
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
    return Dot { n: d.n + 1 }
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
    e = Dot { n: 99 } // `let` bindings are reassignable
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
non-null value. `unwrap()` and `expect("why")` abort on null.

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
Offsets and lengths count bytes. The method list below is the core;
`upper`, `lower`, `repeat`, `starts_with`, `ends_with`, `replace`,
`pad_left`, `pad_right`, `lines` and `parse_float` work the same way.

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
and `json_field` pull typed data out.

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

## Modules and use

A project is a directory with a `stick.toml`. By default its `.rock`
files use the legacy flat namespace, so helpers need no import. A package
can opt into scoped modules:

```toml
[package]
name = "greeter"

[language]
modules = "v1"
```

In a v1 package, a `mod` header assigns a file to a logical module;
without a header it belongs to the package root. Multiple files can
contribute to the same module. Items are private to their defining module
and its descendants unless exported. Here `super::` addresses the parent
module, so `main` reaches its sibling through `super::helpers::shout`:

```rock
// This file contributes to the logical module named by its `mod` header,
// then imports what it needs with `use`. Run the whole directory:
//
//     rock run .
//
mod main;

use super::helpers::shout;

fn main() {
    print(shout("hello"))
}
```

```rock
// Items default to visibility in their module and its descendants.
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
imports, visibility, and compatibility with legacy packages.

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
to JavaScript instead and runs it with `node`, which must be on `PATH`;
`ROCK_BACKEND=js` makes that the default. `rock check` only typechecks.

`rock compile` emits the Lua without running it; `rock shader`
emits GLSL as described above.

Set `ROCK_LUA_TRACE_FRAMES=1` to report final emitted Lua function frames,
direct call register prefixes, and non-tail call-chain slot pressure. Select
the target VM with `ROCK_LUAJIT`, or put `luajit` on `PATH`. The diagnostic
parses the generated Lua without running it and leaves emitted code unchanged.
It writes to stderr, or to `ROCK_LUA_TRACE_FRAME_REPORT` when set. Warnings
name wide scalar parameters and returns, including `nobox` contributors.
Recursion, indirect calls and tail calls leave the full chain unknown;
branches, trace entry and LuaJIT's inlining choices also affect actual aborts.
The report does not change compile errors or the default boxing policy.

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
