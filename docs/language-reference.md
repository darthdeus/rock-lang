# Language reference

This reference covers selected advanced features. Start with the
[language guide](guide.md) for the basic syntax and types.

## No-box values

Mark a value struct `nobox` to require that the compiler does not implicitly
box values of that type:

```rock
nobox value Point { x: float, y: float }
```

The compiler reports an error when a use would require implicit boxing. For a
boundary that needs a boxed value, such as an opaque foreign call or `inspect`,
write `materialize(point)` explicitly. Materialization permits boxing; an
optimizer may eliminate a box whose allocation is not observable. Some
otherwise unboxed uses may still be rejected when their representation is not
supported.

## Operators and references

Binary `+`, `-`, `*`, `/`, `%`, and `./` support user overloads. Other binary
operator definitions are rejected; equality is not user-overloadable. Prefix
`-` and `!` have separate callable families. The
[operator table and tests](../crates/bone-semantic/tests/operator_table.rs)
keep declarations aligned with dispatch.

Numbers follow LuaJIT on every backend. `./` floors and `%` is
`a - floor(a / b) * b`, so the remainder takes the divisor's sign. Dividing by
zero is not an error: it yields `inf`, `-inf` or `nan`, for `uint` too.
For `int`, bitwise operators work on signed 32-bit words: an operand wraps
modulo 2^32 and a shift count uses its low 5 bits. `>>` is a logical shift.
An `int` bitwise operand of magnitude 2^51 or more gives a backend-specific
result. This 32-bit behavior does not describe ordinary `int` arithmetic.

An `int` field of a value struct is a signed 32-bit integer, in
`-2147483648..=2147483647`; a `uint` field is unsigned 32-bit, in
`0..=4294967295`. This rule also applies to unboxed and scalarized values.
Out-of-range literals stored into these fields are compile errors. With
`ROCK_MIR_CHECKS=1`, stores also reject runtime numbers outside the field's
range or with a fractional part (including infinities and NaN). Unchecked
builds require the program to keep stores within these limits. Value structs
keep their C `int` / `uint32_t` field layout for FFI and shaders.

For `uint`, `+`, `-`, `*` and bitwise results wrap as unsigned 32-bit values.
Shifts require a count in `0..=31`: invalid literal counts are rejected during
checking, and invalid dynamic counts fail at runtime. The counts do not wrap.

Whether
arithmetic such as `0.0 * -1.0` yields `-0` depends on LuaJIT's platform, so
do not rely on it. See [numeric edge tests](../tests/operators/numeric_edges.rock).

Reading a field of a non-nil Rock value cannot fail. The compiler may omit
physical reads whose results are unused, including nullable-child reads after
the child's nil check. Inline Lua that attaches metatables or getters to Rock
values is unsupported. Value copies still snapshot the used fields before
later mutations.

`ref T` accesses a place without copying its value; field writes go through
to that place. A function can return a reference derived from a valid input
reference or its field, but not to a local place. The compiler rejects refs
in persistent storage, across opaque boundaries, or live across coroutine
suspension. A coroutine may retain a reference argument between construction
and first resume; see [coroutine reference lifetimes](coroutines.md).
A relocatable collection element can be borrowed for an immediate
call but cannot be retained. See [reference fixtures](../tests/values/refs.rock)
and [admission tests](../crates/bone-semantic/tests/structural.rs).

`as` checks an independently inferred operand against the target type; it allows
assignment-compatible types and numeric conversions. Contextual type ascription
supplies a type to the operand: `(expression : Type)`. `::` separates module
path segments.
Use `cast<T>` to extract a nominal value from `any`.

## Function forms and static locals

A receiver can be named at the definition: `fn Point.move(p, dx: int)`
declares an ordinary function whose first parameter has type `Point`. The
receiver has no type annotation at that position; both `p.move(1)` and
`move(p, 1)` use the same function and overload rules.

Overloads of one name must differ in their parameter types. A second
definition with the same parameter types is rejected even when only its
return type or parameter names differ. Per-backend variants (`inline(js)`
beside `inline(lua)`) are one function, not overloads.

A named function can be declared inside a function. It is visible from its
declaration to the end of its block and can refer to globals, in-scope static
locals and earlier nested functions. It cannot capture enclosing parameters or
locals; use a lambda for those. Self-recursion is supported, but forward and
mutual recursion, generic parameters and annotations on nested functions are
not.

A body-less `extern` declaration names its backend: `extern(lua) fn` refers to
an existing Lua global, `extern(js) fn` to a JS global, and
`extern(ffi, "lib") fn` to a C symbol reached through LuaJIT FFI. A bare
`extern` or `extern(c)` is rejected; both meant `extern(lua)`.

A global may omit its initializer:

```rock
global current_scene: Scene
global selection: Entity?
```

In checked builds (`ROCK_MIR_CHECKS=1`), reading a non-optional global
without an initializer before its first assignment raises a runtime error
naming the global and the read's source file, line and column. This applies
across modules and inside callbacks when they run. Assign the global before
any consumer reads it. Compatible hot reload preserves assigned globals.
An explicitly optional global (`T?`) starts as `null` and may be read before
assignment. Globals with initializers use their initialized values.
Unchecked builds (`ROCK_MIR_CHECKS=0`) keep the existing storage behavior
without these read checks. No declaration syntax changes are needed.

A `static` local persists across calls and initializes the first time control
reaches its declaration:

```rock
fn counter() -> int {
    static n = 0
    n = n + 1
    return n
}
```

Each declaration has one cell shared across calls and loop iterations. Its
initializer cannot read enclosing parameters, locals or another static. Static
locals are rejected in `@pure` and coroutine functions. A shared scratch cell
can be overwritten by recursive or interleaved calls.

## Purity

`@pure` on a function makes the compiler reject, in its Rock body, reads of
mutable globals, assignments and increments, index writes, array and heap
struct literals, lambdas, nested functions, statics, calls through function
values, and calls to functions that are neither `@pure` nor `@debug_impure`.
Two things it does not establish: an `inline(lua)`, `inline(js)` or `extern`
body marked `@pure` is trusted as written, and a `@debug_impure` callee (the
escape hatch for `print`) may do anything. So `@pure` is a contract the
checker holds the Rock body to, not a proof that a call leaves every value it
can reach unchanged. Type refinements (null narrowing across a call) rely on
it; value-level optimizations must not. `docs/purity.md` is the audit of
every consumer and the escape hatches.

Two annotations describe an opaque body's writes for the optimizer, since
nothing can read an `inline(lua)` or `extern(lua)` body: `@nowrite` says it
stores to no table field, element or global the program can reach, including
NativeBuffer storage (IO, allocation and raising are fine), `@writes_elems` that it stores
only into elements of containers it is handed. Calls into `extern(ffi, ..)`
may write native elements. If any binding in that library accepts a callback
or an `any` argument, all calls into that library may also mutate Lua state. The compiler infers the same information for Rock bodies by
reading them (`bone_core::effects`), so the annotations are ignored there.
Two more describe an opaque helper's integer result for the bounds-check
elision: `@returns_index_below("<param>[.field]*")`, the result is below the
live length of the vector that place holds when the call returns, and
`@returns_nonneg`. The standard library's column-map slot helpers carry
them; they are promises of the same kind as the body itself.

## Macros

Macros expand at call sites and can accept syntax fragments. Arguments are
substituted into the body, so an expression used twice can be evaluated twice:

```rock
macro double(x: expr) -> int { x + x }
let result = double(5)
```

Macros can be overloaded by arity. Two fixed-arity overloads, or two variadic
ones, that accept the same argument count are rejected; a fixed-arity overload
takes precedence over a variadic one. A variadic macro can
expand recursively using `rest: ...expr` and `...rest`. A macro body can use
`_G.name` for an explicit global and `^name` for a binding in the caller's
scope. Macro-introduced local bindings do not capture substituted arguments.
Macros currently use global lookup; see
[Modules](modules.md).

## Embedded fields

An `embed` field is owned by its containing struct and is projection-only:

```rock
struct Point { x: int, y: int }
struct Holder { embed p: Point }
```

Leaf access and assignment, whole-field constructor assignment, and a direct
scalar-ABI round trip such as `h.p = advance(h.p)` are allowed. An embedded
field cannot be independently bound, returned, captured, or passed through an
`any` boundary. A nullable embedded field permits presence tests and leaf
safe-navigation; coalescing the whole field is rejected. Field selection of an
embedded field uses the write-only `WriteFieldRef`, not a readable field
reference. Only nominal structs can be embedded, and embed graphs must be
acyclic. `embed` is contextual: `embed: int` remains a field named `embed`.

## Narrow floating-point storage

`float` and `f64` are the same 64-bit type. `f32` is narrow storage for struct
and value fields only; reads and writes use `float`, and each write rounds to
single precision. It is not a general local, parameter or generic argument type.

## String literals and trailing blocks

Ordinary quoted strings process escapes such as `\n`, `\t`, `\r`, `\\` and
`\u{...}`; unknown escapes are errors. Raw strings such as `r"{name}\n"`
preserve their contents without escape processing or interpolation. Raw strings
can also be triple quoted for multiline text.

`f(args) @ expression` appends a zero-parameter lambda as the last argument:
`f(args, () => expression)`. The right side can be a block, and chained `@`
expressions nest from the right. The callee decides when to invoke the lambda;
this syntax does not immediately evaluate its body.

## Default type parameters

Structs, enums and newtypes may give trailing type parameters defaults:

```rock
struct Pair<T, U = int> { first: T, second: U }
let pair: Pair<string> = Pair { first: "one", second: 1 }
```

`Pair<string>` means `Pair<string, int>`. If every parameter has a default,
the type name can be used without arguments. Defaults must follow all required
parameters and must be builtin types or bare type names. Builtin defaults
include `uint` and `f64` (the alias of `float`); `f32` remains field-storage-only.
Defaults cannot refer to another parameter, be nullable or be generic
applications. Function type parameters do not support defaults.

A newtype wraps exactly one type. To wrap several values, use a tuple such as
`newtype PairId((int, string))` or define a struct.

## Patterns in `let` and `for`

`let` and `for` bind through the same patterns `match` uses. Without an
`else` the pattern must be irrefutable -- provably matching every value of
its type: names, `_`, tuples, struct records (`let Point { x, .. } = p`),
newtypes (`let Id(n) = id`), and their nestings. Refutability is decided by
the match exhaustiveness checker on the one-pattern matrix, so a
single-variant enum pattern is irrefutable too.

`let <pattern> = value else { ... }` accepts refutable patterns and
nullable scrutinees: a nullable value unwraps exactly as in `if let`, the
`else` block runs on a miss and must diverge (its type must be `never`:
end with `return`, `break`, `continue`, or a `loop`/`while true` that
never breaks; Rock has no never-returning call to end it with), and
the bindings join the enclosing scope. An `else` that can never run (an
irrefutable pattern over a non-nullable value) is rejected. `let ... else`
is a statement; in expression position use `if let` or `match`.

`while let P = v { body }` is exactly
`while true { let P = v else { break } body }` and is compiled as that
desugaring, so the two forms share every rule above. A `while let` whose
pattern can never miss over a non-nullable value is rejected (an infinite
loop by construction), and its scrutinee parses at the same precedence as
an `if let` clause, so a trailing `&& cond` is a parse error.

The `match` rule for a bare uppercase name holds in every pattern
position: against an enum scrutinee it is a variant tag test that binds
nothing (a misspelled variant is an error, never a catch-all), and
against any other type it is an ordinary binding. Structural patterns
look through `T?` everywhere, including `match`: `null` is its own
pattern, and a variant/record/newtype/tuple pattern matches the
unwrapped value.

A `defer` body runs while its scope is already leaving, so it is outside
any enclosing loop and cannot return from the function: `return` in a
deferred block is rejected, and so are `break`/`continue` aimed at a loop
outside it. Control flow that stays inside the body (a loop the block
owns, a lambda returning from itself) is fine. A lambda body is likewise
not the enclosing loop's body: `break`/`continue` inside a lambda are
rejected even when the lambda is written inside a loop.

## Iterators

A collection can define `iter(collection)` to return an iterator. An iterator
provides `next(iterator) -> (bool, T)`: the boolean says whether the returned
value is present. A `for` loop calls these operations; an iterator with `next`
can also be used directly. A generic function can require this protocol with
`I: Iterator<T>`. A multi-value `next` can drive a direct `for`, but does
not satisfy the two-result `(bool, T)` bound.

A struct marked `@consume` has single-use bindings. Standard-library cursor
structs carry this annotation. A generic parameter bounded by `Iterator<T>`
follows the same rules; unbounded generic parameters remain reusable.
A whole-value read consumes the binding: this
includes a `for` head, passing an argument or method receiver, copying to another
binding, storing in a field or Vec, returning, and capturing in a lambda. Any
later read of that binding is an error, with the first consumption also labeled.
Field projection and `next(cursor)` / `cursor.next()` do not consume a fresh
binding, so manual advancement can precede a final loop or terminal operation.

Reassignment and shadowing provide fresh bindings. Exclusive branches may each
consume a cursor, but consumption in either branch makes it unavailable after
the join. A loop cannot consume a binding declared outside that loop; create a
fresh cursor inside each iteration or advance it with `next`. Parameters start
fresh in each body, including `co fn` bodies; retaining a cursor across `yield`
is allowed. Tracking is per binding, not through fields or across calls.
Ranges and collections remain reusable; each call to `iter()` creates a fresh
cursor.

```rock
let result = [1, 2, 3, 4].iter()
    .filter(x => x % 2 == 0)
    .map(x => x * 10)
    .collect()
```

Combinators such as `map`, `filter`, `take`, `skip`, `chain` and `flat_map`
are lazy. Terminals such as `collect`, `reduce`, `find`, `any`, `all`, `count`
and `sum` consume the iterator. A collection can be passed directly to an
iterator-bounded function when its `iter` method supplies the iterator.
The same holds for a lambda's result where the callee expects an iterator,
so `[1, 2].flat_map(n => [n, n])` iterates each returned Vec; a named fn
returning a Vec goes through a lambda (`n => pair(n)`).
## Compile-time inputs

Declare build variables in the project's `stick.toml`:

```toml
[build.vars]
CHANNEL = "development"
```

A direct `build_var("CHANNEL")` call expands to the declared string. The CLI
can override a declared variable with `--var NAME=value`. A direct
`embed_string("data/message.txt")` call embeds UTF-8 text from a file. Its
path is relative to the calling file's package root (the nearest `stick.toml`)
and cannot escape that root. Both calls require string literals; undeclared
variables and missing files are errors. These values are embedded in the
program: do not treat them as secrets.

## Typed pointers

`Ptr<T>` and the sized pointer types `PtrU8`, `PtrI8`, `PtrU16`, `PtrI16` and
`PtrU32` distinguish FFI pointer types at compile time. `NativeBuffer<T>.ptr`
has type `Ptr<T>`; sized buffers use their corresponding sized pointer type.
Pointer values refer to existing storage, so copying one does not copy its
contents. Their runtime representation depends on the backend. For scalar
out-parameters, `ffi_out_int()` and `ffi_out_float()` create pointers that can
be read with `read()`.

## Local type holes

A `?` type annotation asks the compiler to infer that type from its context:

```rock
let count: ? = 42
```

Tests can assert inferred hole types with `@test(type_holes = [[0, "int"]])`;
see the [testing guide](testing.md). Prefer an explicit type when inference
cannot establish the intended type.
