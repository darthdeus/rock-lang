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
Bitwise operators work on signed 32-bit words: an operand wraps modulo 2^32
and a shift count uses its low 5 bits. `>>` is a logical shift. An operand of
magnitude 2^51 or more gives a backend-specific result. Whether
arithmetic such as `0.0 * -1.0` yields `-0` depends on LuaJIT's platform, so
do not rely on it. See [numeric edge tests](../tests/operators/numeric_edges.rock).

`ref T` accesses a place without copying its value; field writes go through
to that place. A function can return a reference derived from a valid input
reference or its field, but not to a local place. The compiler rejects refs
in persistent storage, across opaque boundaries, or live across coroutine
suspension. A relocatable collection element can be borrowed for an immediate
call but cannot be retained. See [reference fixtures](../tests/values/refs.rock)
and [admission tests](../crates/bone-semantic/tests/structural.rs).

## Function forms and static locals

A receiver can be named at the definition: `fn Point.move(p, dx: int)`
declares an ordinary function whose first parameter has type `Point`. The
receiver has no type annotation at that position; both `p.move(1)` and
`move(p, 1)` use the same function and overload rules.

A named function can be declared inside a function. It is visible from its
declaration to the end of its block and can refer to globals, in-scope static
locals and earlier nested functions. It cannot capture enclosing parameters or
locals; use a lambda for those. Self-recursion is supported, but forward and
mutual recursion, generic parameters and annotations on nested functions are
not.

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

## Macros

Macros expand at call sites and can accept syntax fragments. Arguments are
substituted into the body, so an expression used twice can be evaluated twice:

```rock
macro double(x: expr) -> int { x + x }
let result = double(5)
```

Macros can be overloaded by argument shape and arity, and a variadic macro can
expand recursively using `rest: ...expr` and `...rest`. A macro body can use
`_G.name` for an explicit global and `^name` for a binding in the caller's
scope. Macro-introduced local bindings do not capture substituted arguments.
Macros currently use legacy global lookup even in modules v1; see
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
parameters and must be builtin types or bare type names. They cannot refer to
another parameter, be nullable or be generic applications. Function type
parameters do not support defaults.

A newtype wraps exactly one type. To wrap several values, use a tuple such as
`newtype PairId((int, string))` or define a struct.

## Iterators

A collection can define `iter(collection)` to return an iterator. An iterator
provides `next(iterator) -> (bool, T)`: the boolean says whether the returned
value is present. A `for` loop calls these operations; an iterator with `next`
can also be used directly. A generic function can require this protocol with
`I: Iterator<T>`.

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
Iterator-bounded calls in coroutine bodies are currently rejected.

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
