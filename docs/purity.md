# Purity and effects

`@pure` is a source-level checker contract. A Rock body marked pure cannot
mutate state or call an ordinary impure function. Opaque `inline` and `extern`
bodies cannot be inspected, so their annotations are promises by their author.
`@debug_impure` is an escape hatch intended for debug printing.

Nullability narrowing may trust `@pure`: a call to a pure function preserves
refinements. Value-level optimizations instead use effects inferred from Rock
bodies and their callees, including effects hidden behind a `@debug_impure`
call. The effect analysis must trust annotations on opaque bodies because it
cannot inspect their target code.

`@nowrite` promises that an opaque body writes no reachable table field,
element, global or native-buffer storage. `@writes_elems` limits its writes to
elements of containers passed to it. A seemingly read-only operation may still
invoke a callback, such as a Lua metatable or a host-installed function; such
operations cannot make these promises unless that callback is excluded. Wrong
promises can invalidate optimization assumptions and lead to incorrect results.
