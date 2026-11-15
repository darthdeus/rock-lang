# Static local and nested function rejection evidence

The seven [static-local](../tests/errors/static_locals.rock.err) cases and eight
[nested-function](../tests/errors/nested_fns.rock.err) cases now pin diagnostic
reasons. Fourteen previously accepted any rejection; the existing dynamic
format-precision capture assertion now also names the captured binding.

## Rules and implementation

- Static declarations in `@pure` functions reject because the declaration
  writes module state. `verify_pure_stmt` in
  [purity.rs](../crates/bone-inference/src/purity.rs) checks the declaration,
  independently of any later assignment.
- A static directly in a coroutine body rejects in the `Statement::Static`
  branch of [expr.rs](../crates/bone-inference/src/expr.rs).
- Static initializers cannot capture enclosing parameters, locals or sibling
  statics. Nested functions cannot capture enclosing parameters or locals,
  but can reference in-scope statics and prior nested functions. The shared
  `reject_captured_binding` walk in
  [purity.rs](../crates/bone-inference/src/purity.rs) distinguishes these two
  capture contexts. Bindings declared inside the checked construct are allowed.
  The local-shadowing-global case checks resolved binding identity.
- Nested declarations reject generic parameters and annotations in the
  [parser](../crates/skull-parser/src/parser.rs). Their lexical scope and
  declaration order are established in
  [lowering](../crates/bone-lower/src/lib.rs): the name is bound before lowering
  its body, admitting self-recursion but not a later sibling.

The static purity fixture had a competing explicit mutation; that assignment
was removed. The coroutine static fixture used its rejected binding afterward,
producing undefined-variable errors; it now yields a literal. The nested
parameter-capture fixture also called `len` on a `FilterIter`, which independently
rejected; it now calls the nested predicate directly. These repairs preserve the
intended violations while removing unrelated rejection causes. The nested
generic declaration still produces a parser recovery diagnostic as well as the
specific asserted restriction.

The generated-looking `__static_f_n` name rejects as an undefined global. It is
not the current emitted cell spelling: `lower_static` in
[MIR statement lowering](../crates/skull-mir/src/lower/stmt.rs) includes an owner
symbol and a counter. This one name-resolution case does not prove generated
cell privacy; its comment now states that limit.

## Controls and results

[Static runtime tests](../tests/functions/static_locals.rock) exercise persistent,
distinct cells, loops and first-reach initialization. The
[nested-function tests](../tests/functions/nested_fns.rock) exercise direct and
function-value calls, self-recursion, shadowing, sibling scopes and static access.
Four new [capture controls](../tests/functions/hoisted_capture_controls.rock)
check a static snapshot of a global, locals and a lambda created inside an
initializer, an enclosing value passed explicitly to a nested function, and a
prior sibling call that reads a global. Stale feature-not-yet-landed comments
were removed from the corpora.

```sh
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/static_locals.rock.err --cases 7
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/nested_fns.rock.err --cases 8
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/functions/static_locals.rock tests/functions/nested_fns.rock tests/functions/hoisted_capture_controls.rock
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/functions/static_locals.rock tests/functions/nested_fns.rock tests/functions/hoisted_capture_controls.rock --backend js
```

All 15 originals pass. Deliberately wrong diagnostics and valid replacement
programs each fail all 15 assertions. The runtime files pass 19/19 tests on
LuaJIT and 19/19 on JS, with zero JS skips or not-for-JS cases. These checks do
not audit every capture path, generated-name collision, static reload behavior
or generic specialization. No production compiler code changed.
