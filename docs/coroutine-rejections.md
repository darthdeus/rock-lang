# Coroutine rejection evidence

Ten existing cases now assert their rejection reason. Nine already rejected for
what their comments described. The tenth exposed a stale iterator restriction:
[coroutine_result_type.rock.err](../tests/errors/coroutine_result_type.rock.err)
(previously `iterators/bounded_call_in_nested_body.rock.err`) declared a `co fn`
with result annotation `int`. Inference rejected that annotation before the
bounded call could reach emission. Correcting the annotation to
`Coroutine<int, int>` and replacing interpolation of the coroutine with
`resume` admits the program. The new
[bounded-chain runtime control](../tests/coroutines/coroutine_bound_calls.rock)
checks that its result is 2 on LuaJIT and JS. The original invalid input remains
as a diagnostic-pinned signature rejection; it no longer claims to check a
residual bounded-call gate.

| Boundary | Rejections | Implementation | Accepted controls |
|---|---|---|---|
| Reachable end with a concrete non-unit, non-nullable result | Four in `coroutine_falloff_nonunit.rock.err`, including generic and forwarded instantiations | `reject_nonunit_coroutine_falloff` in [MIR lowering](../crates/skull-mir/src/lower/mod.rs) | Nullable/unit generic fall-through and explicit concrete returns in `generic_result_falloff.rock` |
| Plain lambdas cannot yield, even inside a coroutine | Three in `yield_in_plain_lambda.rock.err` | Yield checking in [inference](../crates/bone-inference/src/expr.rs) | Closures called across yields in `closure_captures.rock` |
| The standard iterator adapter requires unit resume arguments | One in `coroutine_next_resume_arg.rock.err` | `next<Y, R>` in [stdlib](../stdlib/unified.rock), protocol resolution in [inference](../crates/bone-inference/src/iter.rs) | `generator_iter.rock` and explicit arguments in `resume_with_value.rock` |
| Delegation uses `await`, not the removed `yield*` spelling | One in `yield_star_removed.rock.err` | [Parser](../crates/skull-parser/src/parser.rs) | `await.rock` |
| A coroutine result annotation must name `Coroutine` | One in `coroutine_result_type.rock.err` | `coroutine_yrn` in [inference](../crates/bone-inference/src/expr.rs) | `coroutine_bound_calls.rock` |

For each rejection file, run `python3 perf/evidence/check_operator_rejections.py
--fixture tests/errors/FILE.rock.err --cases N` with the table's case count.
All ten originals pass; deliberately wrong diagnostics and valid replacement
programs each fail all ten assertions. The six accepted-control files pass
28/28 tests on LuaJIT and 28/28 on JS (zero skipped or not-for-JS cases). The
commands and nonzero count assertions are in [the ledger](evidence.json).

These are bounded examples, not a complete coroutine or specialization audit.
In particular, the corrected bounded chain does not establish acceptance of
all recursive or residual bounded calls. The iterator adapter observation is
about the standard overload, not a prohibition on user adapters. The removed
syntax diagnostic still has a confusing prefix saying that `await` was renamed
to itself; this audit pins its actionable `write await expr` instruction (with
backticks in the actual diagnostic). No production compiler behavior changed.
