# References across coroutine suspension

## Decision and established boundary

Preserve the established resume-segment rule. A reference acquired in a segment
can be used through its final use in that segment. A later use reached through
`yield` requires a fresh reference. A copied scalar or an `Entity<T>` handle can
survive suspension, and the handle can resolve a fresh borrow on resumption.

Commit `968fb923` introduced explicit frame refs together with both original
boundary cases. A `ref Player` parameter used after `yield` rejects, and the
same parameter finished before `yield` accepts. The current reference and
engine contract describe borrows live across suspension. This audit preserves
those source-level semantics. [Language reference](language-reference.md#operators-and-references),
[engine contract](engine.md)

Implicit identity parameters have an additional storage rule. Coroutine
construction retains a parameter before the first resume, so `player: Player`
is rejected even when its last use precedes the first `yield`. An identity local
can finish before suspension. Its later use across suspension rejects with the
identity-specific diagnostic. [Structural tests](../crates/bone-semantic/tests/structural.rs),
[identity fixtures](../crates/bone-semantic/tests/fixtures/identity_storage_bodies.rock)

## Implementation

| Responsibility | Implementation |
|---|---|
| Bind, suspend, merge branches, iterate loop flow, follow exits and deferred uses | `uses_after_suspension` and `Walk` in [shared suspension traversal](../crates/bone-core/src/suspension.rs) |
| Select explicit-ref bindings and report frame-borrow diagnostics | `RefSuspensionPolicy` and `check_function` in [inference](../crates/bone-inference/src/lib.rs) |
| Select implicit identity locals and reject coroutine identity parameters | `IdentityBorrowPolicy` and `identity_body_sites_with` in [identity storage](../crates/bone-core/src/identity_storage/sites.rs) |
| Lower and execute admitted coroutine bodies | [coroutine transformation](../crates/skull-transform/src/coroutine.rs) |

The two checks already share control-flow traversal. Their binding selection
and parameter admission rules have different responsibilities. This audit
found no production simplification justified by the selected boundary.

## Checks and results

The original generic explicit-ref rejection and acceptance examples moved into
a dedicated semantic fixture. Its 12 sections cover parameter and local refs,
aliases, conditional suspension, loop backedges, deferred uses, fresh acquisition,
assignment after suspension, terminating branches and per-iteration acquisition.
All six rejection sections pin `frame borrow is used after a coroutine
suspension`. [Semantic fixture](../crates/bone-semantic/tests/fixtures/reference_suspension.rock)

Six CLI rejection cases repeat the unsafe shapes through the normal front end.
Four runtime cases check copied values, writes through a fresh per-iteration
ref, and handle resolution after mutation or removal between resumes.
[CLI rejections](../tests/errors/reference_suspension.rock.err),
[runtime cases](../tests/coroutines/reference_suspension.rock)

Executed checks on 2026-11-15

- `cargo test -p bone-semantic` passed 109 integration tests and 163 fixture-file
  tests. The dedicated fixture counts as one test with 12 sections.
- `cargo test -p bone-semantic --test fixtures check_rock::reference_suspension.rock -- --exact`
  passed one fixture-file test.
- An isolated copy of the CLI rejection file passed six cases with zero TODOs.
  All six failed with deliberately wrong diagnostic expectations, and all six
  failed when replaced by valid programs. The error runner scans its input's
  parent directory, so isolation preserves that exact count.
- `cargo run --quiet --bin rock -- test tests/coroutines/reference_suspension.rock`
  passed four runtime cases under LuaJIT.
- The same command with `--backend js` passed four cases with zero skips.

Temporarily forcing `RefSuspensionPolicy::binding_holds_frame_data` to return
false made the dedicated fixture fail on its first rejection section. All six
isolated CLI rejection cases also failed because both the front end and emitter
accepted their programs. The condition was restored and checks rerun. This
mutation establishes that these checks depend on the explicit-ref suspension
condition. Its coverage is limited to the selected paths.

The changes strengthen tests and document the selected boundary. Compilation
and runtime performance remain unmeasured in this audit.

## Open questions

Explicit-ref parameters retain the original pre-first-yield acceptance despite
the identity-parameter storage rationale above. A normal CLI runtime probe
confirmed deferred execution with an explicitly typed local ref

```rock
value Cell { hp: int }
co fn read(cell: ref Cell) -> Coroutine<int> { yield cell.hp }
@test
fn deferred_read() {
    let cell = Cell { hp: 1 }
    let borrowed: ref Cell = cell
    let task = read(borrowed)
    cell.hp = 19
    assert_eq(task.resume() ?? -1, 19)
}
```

The probe passed on LuaJIT. Coroutine construction therefore retains this ref
before the first resume. The direct call `read(cell)` was rejected during
overload selection, while the explicit-ref local above was accepted. A future
change needs to settle whether coroutine reference parameters must be rejected
at construction and how calls should explain this boundary. The current audit
records this discrepancy alongside the established acceptance case. Further
lifetime-audit targets include nested coroutine functions,
closure-held borrows and additional suspension forms.
