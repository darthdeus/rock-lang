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
