# Embed projection rejection evidence

An embedded child has no independently storable whole value, even when a
particular layout could leave it boxed. This is the language rule introduced
in [53efd426](https://github.com/darthdeus/rock-lang/commit/53efd426f2b4d4e56af4e3eb34e46867b72cce59),
not a restriction inferred from today's diagnostic wording.

[`legal_positions`, `scalar_abi_callee` and `verify_embed_projection`](../crates/bone-inference/src/embed.rs)
implement an allow-list: leaf projection, place writes, equality/presence tests,
interpolation and direct Rock-bodied scalar-ABI calls confer legality. Binding,
return, closure result, collection element, `any` erasure, whole-value nullable
`?.let`/`??`, and opaque or function-value callees do not. The fixture's one
opaque-callee case combines `@nomangle` and `inline(lua)`; it does not independently
exercise every opaque-call category. The lambda case returns `h.p`, so it checks
whole-value extraction in a lambda; it does not establish that capturing the
`Holder` or reading embedded leaves in a lambda is forbidden.

[Field-selector admission in `check_expr`](../crates/bone-inference/src/expr.rs)
separately refuses embedded `.p` as `ReadFieldRef` or `FieldRef` because either
permits whole-value extraction. `WriteFieldRef` remains admissible; the
[`coerce` capability lattice](../crates/bone-inference/src/unify.rs) never grants
read capability from it.

The ten cases in [`embed_projection_only.rock.err`](../tests/errors/embed_projection_only.rock.err)
now assert one intended diagnostic each: eight whole-value uses require
`` `p` is an `embed` field: it has no independently storable value ``; the two
readable-selector cases require `a readable selector would hand out the whole
value`. All ten reached their intended diagnostic without fixture isolation
changes. The assertions identify the rejection family, rather than matching
punctuation or the entire help text.

Verified on 2026-11-15:

| Command | Observed result |
| --- | --- |
| `python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/embed_projection_only.rock.err --cases 10` | Original: 10 passed, 0 failed; wrong diagnostic: 0 passed, 10 failed; generic valid programs: 0 passed, 10 failed; all 0 todo |
| `cargo run --quiet --locked -p rock-cli --bin rock -- test tests/values/embed.rock` | Lua: 39/39 passed, 0 failed; all 39 annotations ran, none excluded |
| Same value-test command with `--backend js` | JS: 35/35 passed, 0 failed, 0 skipped, 4 Lua-only |

The [positive fixture](../tests/values/embed.rock) covers leaf reads/writes,
whole-field construction writes, direct-call round trips, equality, nullable
presence and safe leaf projection. Two bounded controls now also check actual
embedded `.p` admission as `WriteFieldRef`, and plain `.p` admission as both
readable selector forms. They test selector admission, not runtime write-through
of an embedded selector. Changing only these two output expectations in a
temporary copy produced exactly two failures on each backend (Lua 37/39, JS
33/35, with the same four JS exclusions), naming both new tests. The checked-in
expectations then passed on both backends.

Limits: this is a ten-case rejection audit, not exhaustive enforcement mutation
coverage. No production code was changed. The obsolete introductory claim that
storage is not flattened was corrected. The value fixture still explicitly
preserves an inverted flat-enum reflection sentinel.
Its green count therefore does not establish general layout/reflection
correctness or validate that sentinel's desired behavior.
