# Map key rejection evidence

The intended rule is representation-based: scalar keys, scalar-backed newtypes,
and payload-free enums are admitted; aggregate keys, payload-carrying enums,
and newtypes wrapping rejected keys are rejected. Equal fresh aggregate keys
otherwise miss because the backends use identity. Commit `248f5923` records
the measured tuple miss, the broadened rule, and why unresolved generic keys
must be deferred. [`map_key_rejection_reason`](../crates/skull-mir/src/lower/mod.rs) implements the shared
check; Lua and JS emission invoke it. [The error fixture](../tests/errors/map_value_struct_key.rock.err) now pins each of its
nine cases to the corresponding key reason, including inferred value keys,
`Entity`, both aggregate types, both struct forms, payload enums, and two
newtype wrappers. Its historical filename understates that scope.

Run `python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/map_value_struct_key.rock.err --cases 9`.
On this checkout the originals passed 9/9, deliberately wrong diagnostic
substrings failed 9/9, and valid replacement programs failed 9/9. Each
expectation is a diagnostic substring, not an exact message or span assertion.
The test proves these examples reach the intended rejection, not that all
possible uses of each type are rejected.

Positive runtime controls: [newtype keys](../tests/newtypes/map_keys.rock) pass
10/10 on Lua and JS, covering int and string newtype keys, including lookups
with freshly constructed equal keys. [map basics](../tests/maps/basics.rock) and [enum equality](../tests/patterns/enum_eq.rock) pass 46/46 on each backend, including scalar
maps and a payload-free enum map. JS reports zero skipped and zero not-for-JS
in both runs; Lua reports no skip count. The JS emission unit controls in
[map-key admission tests](../crates/skull-codegen-js/tests/map_key_admission.rs) also check admitted and
rejected examples.

Limits: the implementation examines inferred expression types in function
bodies, defers unresolved keys, and stops following newtypes after depth eight;
this fixture does not establish rejection in unexamined contexts or deep
wrappers. The intended recursive rule in the historical decision should not be
read as proof that every such case is enforced. The fixture also does not cover
nullable, function, nested Map, or reference keys, nor every scalar type or
backend-specific representation. No production behavior changed here.

Rebuild and rerun the accepted controls from this checkout:

```sh
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/newtypes/map_keys.rock
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/maps/basics.rock tests/patterns/enum_eq.rock
```

Repeat each command with `--backend js` for JavaScript.
