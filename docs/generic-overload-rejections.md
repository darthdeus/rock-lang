# Generic arguments and overload rejection evidence

This audit checks 12 cases: eight previously unpinned generic/overload
rejections, two already-pinned newtype inference cases, and two new no-match
overload cases. No production compiler behavior changed.

| Boundary | Cases | Implementation |
|---|---|---|
| Required generic arguments in struct, enum, Entity and Arena annotations, including function parameters | Five in [generic_validation.rock.err](../tests/errors/generics/generic_validation.rock.err) | `lower_resolved_type_name` in [lowering](../crates/bone-lower/src/lib.rs) checks minimum arity after accounting for defaults |
| Entity and user-struct type arguments do not inherit scalar int-to-float widening | Two in [variance.rock.err](../tests/errors/generics/variance.rock.err) | `coerce_impl` and recursive `unify` in [unify.rs](../crates/bone-inference/src/unify.rs) |
| Incomparable generic candidates tie for `(int, int)` | One in [ambiguous_overload_incomparable.rock.err](../tests/errors/ambiguous_overload_incomparable.rock.err) | `select_overload_with_generics` and `param_genericity` in [calls.rs](../crates/bone-inference/src/calls.rs) |
| No candidate matches the supplied types or argument count | Two new cases in [overload_no_match.rock.err](../tests/errors/overload_no_match.rock.err) | The empty candidate-set diagnostic in [calls.rs](../crates/bone-inference/src/calls.rs) |
| Generic newtype arguments must fit their wrapped type pattern | Two in [generic_unification.rock.err](../tests/errors/newtypes/generic_unification.rock.err) | The newtype-constructor path in [calls.rs](../crates/bone-inference/src/calls.rs) instantiates generics and coerces the argument to the substituted wrapped type |

The missing-argument function fixture accessed the obsolete `Entity._index`
field after its invalid parameter annotations. Its body now returns a literal,
so fixing the annotations would not uncover an unrelated missing-field error.
The newtype cases no longer use the results of already-invalid constructors.
The variance diagnostics name the mismatched inner scalar types; the fixtures
and their accepted controls establish the enclosing generic context.

## Accepted controls and results

Five new [runtime controls](../tests/types/generic_rejection_controls.rock)
check same-type generic arguments alongside scalar widening, explicitly typed
Arena/Entity use in a parameter and return, the incomparable overload pair with
only one candidate matching, valid arguments to the no-match overload set, and
matching newtype constructor patterns. Existing
[generic annotation controls](../tests/types/generic_type_validation_positive.rock)
and [overload tests](../tests/functions/overloads.rock) cover struct/enum use and
concrete-versus-generic ranking, among other cases.

Run `perf/evidence/check_operator_rejections.py` with `--fixture` and `--cases`
for each table row. All 12 originals pass; deliberately wrong diagnostics and
valid replacement programs each fail all 12 assertions. The three runtime
files pass 37/37 tests on LuaJIT and 37/37 on JS, with zero JS skips or
not-for-JS cases. Exact commands and nonzero count assertions are recorded in
[the ledger](evidence.json).

These observations do not establish all generic variance, default-argument,
return-context or overload ranking rules. In particular, the runtime overload
file contains commented error examples; those are not executed rejection tests
and are not certified by its passing test count. The tie assertion checks the
ambiguity reason and argument types, not the full candidate-list rendering.
