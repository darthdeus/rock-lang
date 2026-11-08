# Modules

Modules v1 give a package compile-time namespaces and visibility boundaries.
Select them in `stick.toml`:

```toml
[language]
modules = "v1"
```

The choice is per package. An absent setting or `modules = "legacy"` keeps
legacy flat lookup and syntax; unknown values are errors. Modules do not
create runtime namespace objects or copies of definitions. Aliases share the
same definition and storage. Source visibility does not disable lifecycle
discovery: private hooks remain discoverable, and aliases do not register the
same hook twice. A custom decorator named like a builtin does not acquire the
builtin's trusted identity. `pub` controls source access, not the emitted
symbol's spelling; raw foreign-code references still need the appropriate ABI
and name-retention contract.

## Membership and paths

A file without a header contributes to its package's root module. A header
at the start of a file assigns the rest of that file to a logical module:

```rock
mod combat::casting;
```

The path is relative to the package root, independent of the file's directory.
Several files can contribute to the same module. `mod self;` names the root.
A header is not a request to load a file. Inline `mod name { ... }` blocks
contribute to a child of their enclosing module. Both forms can contribute to
the same module. An explicit visibility on a header declares the visibility of
its final module; conflicting explicit declarations are errors. A bare header
can join an explicitly visible module. Exposing a child does not automatically
expose its ancestors.

`self::` names the current logical module and `super::` its parent; `super::`
cannot escape the package. The package's own manifest name and each direct
dependency key provide root names, with hyphens normalized to underscores.
Paths may name accessible definitions directly, without an import. A dependency
must be declared even when a path is fully qualified; transitive dependencies
are not automatically available as roots. `::name::item` anchors a path at a
root name when a local binding shadows it.

In v1, `::` separates path segments. Contextual type ascription is written
`(expression : Type)`.

## Imports and lookup

```rock
use geometry::Point;
use geometry::shapes::{Circle, square as make_square};
use geometry::shapes::*;
pub use self::api::Point;
pub(package) use self::internal::*;
```

An ordinary top-level `use` is local to its source file and its lexical bodies;
it does not import names into other files in the same module. A block import
applies from its declaration to the end of the block. A visibility-qualified
`use` creates a shared module binding, so `pub use` can expose an accessible
item through a public facade. An import cannot grant more access than the
original definition permits. A glob imports immediate accessible members, not
the contents of child modules recursively. `use geometry;` binds the module
name; it does not open its members.

Lexical bindings shadow outer names. For other unqualified names, lookup
prefers the current module, then explicit file imports, glob imports, and the
standard prelude. Non-callable names shadow lower tiers. Callable overloads
from those tiers instead form a family: the best matching visible overload
wins, with the nearer tier breaking an otherwise equal match. An equal match
within a tier is ambiguous. Imports of the same definition do not duplicate it.
A qualified call selects from the named module's family. Operators and UFCS
calls use the same visibility and overload rules; importing a type alone does
not import its extension functions. Macros retain their legacy global behavior.

## Visibility and fields

Ordinary items default to visibility within their defining module and its
descendants. `private` and `pub(self)` spell that default; `pub(super)` grants
access to the parent subtree, `pub(in path)` to a named ancestor subtree,
`pub(package)` to the package, and `pub` to consumers with an accessible path.
A child can access an ancestor's private item, but a parent cannot access a
child's private item merely by being its parent. Permission does not implicitly
import the name. A public module does not make its contents public.

Struct fields differ: they are public to read and write by default, subject
to the visibility of their type.

```rock
pub struct Account {
    name: string,
    private(set) balance: int = 0,
    private secret: int = 0,
}
```

`private(set)` permits public reads but restricts writes to the defining
module and its descendants; `private` restricts both. A caller must have
write access to explicitly initialize a field, but may omit a restricted field
with a declared default. A required restricted field without a default needs
an accessible factory for outside construction. Setter privacy does not make
a mutable value referenced by the field deeply immutable. Field references
and structural interface conversions cannot grant access that their creation
site lacks; an owner can deliberately delegate an allowed capability.

Public APIs must expose accessible types for their public signatures and fields.
A public item can be re-exported from an otherwise private implementation
module, provided the re-export does not expose a private definition.

## Legacy dependencies

A legacy consumer can use the public root exports of a v1 dependency through
its flat compatibility view; v1 privacy still applies. A v1 package may depend
on a legacy package only when that dependency explicitly opts in:

```toml
[dependencies]
old_lib = { path = "../old-lib", allow_legacy = true }
```

The legacy provider exposes its own declarations through that boundary, not
every name available to its compilation unit. Its bodies still use legacy
lookup, so the boundary does not establish package isolation. Legacy source
can use `use` to import or alias a v1 export while retaining legacy expression
syntax. `rock modules audit <project> --closed` reports reachable legacy
boundaries; it does not prove purity or independence from external effects.
