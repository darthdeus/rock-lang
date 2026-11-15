# Recovered syntax and compiler admission

## Decision

Keep syntax recovery in the parser and strict admission in the compile pipeline.
A recovered tree carries every recorded syntax error. Compilation refuses those
errors before build inputs, lints or emission. Macro expansion can add recovery
errors, so admission checks the expanded trees as well as the source trees.

This is an established boundary. Commit `1969ff2c` added the post-expansion gate
when macro-generated imports were found to disappear. Commit `8a9b7866` moved
the compile stages into `skull::pipeline`. Commit `08c32ceb` arranged the
pre-expansion macro checks beside namespace construction. The audit preserves
these decisions and adds tests at their shared compile entry points.

## Implementation map

| Responsibility | Implementation |
|---|---|
| Return a best-effort tree with recovery diagnostics | `parse_module` in [skull-parser](../crates/skull-parser/src/lib.rs), `Ast::recovery_errors` |
| Require a clean parse for fixture and stdlib loading | `parse_module_clean` in the same file, `stdlib::parse` in [stdlib.rs](../crates/skull-parser/src/stdlib.rs) |
| Reject source recovery errors before host alias resolution | `syntax_errors` and the initial gate in `check_modules` in [pipeline.rs](../crates/skull/src/pipeline.rs) |
| Check macro/function name collisions and argument labels before expansion erases them | `macro_gates` in the pipeline, `semantic_pass_after_gate` in [semantic passes](../crates/bone-semantic/src/semantic_passes.rs) |
| Record unsupported or unexpandable macro uses | `expand_top_level_macros`, `expand_item_macro` and expression expansion in [macro_expand.rs](../crates/skull-parser/src/macro_expand.rs) |
| Prefer expansion recovery errors over downstream semantic errors | The `expanded_syntax` gate in `check_modules` |
| Refuse body errors and reach emission only after the remaining gates | `compile_source` and `compile_modules` in the pipeline |
| Retain source syntax diagnostics across editor updates | `UserFile::syntax_errors`, session initialization, `update` and `diagnostics` in [session.rs](../crates/bone-incremental/src/session.rs) |

The lower-level `pipeline::semantic_pass` exposes the checked workspace and its
recorded errors for tooling. Its callers must choose the admission policy.
The complete one-shot compilation boundary is `compile_source` or
`compile_modules`. The CLI uses these shared entry points in
[skull_backend.rs](../crates/rock-cli/src/skull_backend.rs).

## Executable evidence

[Compiler admission tests](../crates/skull/tests/compiler_admission.rs) call
both shared compile entry points with a recording host. They pin the stage,
diagnostic reason, source identity and host callbacks for the selected failures.
The host delegates accepted programs to real Lua emission.

- A broken statement returns a tree with the following valid binding intact,
  then compilation rejects before resolving aliases.
- Two recovered modules return both diagnostics in module order before any
  host callback.
- An unknown top-level macro and a macro-generated import reject after
  expansion. These examples are particularly important because expansion drops
  their invalid items and leaves an otherwise compilable program.
- An item macro used as an expression reports the expansion reason ahead of
  the semantic error from its unexpanded call.
- Macro/function collisions and unknown argument labels retain their own
  diagnostics before expansion discards definitions or argument labels.
- A tokenizer failure stops at `Stage::Parse` before any host callback.
- Repaired source, a valid item macro and a correctly labeled expression macro
  reach real emission through the expected host phases.

The existing editor recovery test was also run. It updates a `BoneWorkspace`
with a valid function followed by an unfinished function and obtains nonempty
inlay hints. This establishes selected in-process query behavior on recovered
input. Coverage ends at the in-process query API.
[Partial-hint recovery test](../crates/bone-query/tests/cw7_backend_failure_reporting.rs)

## Checks and deliberate breaks

Commands run during this audit on the restored implementation

```sh
cargo test -p skull compiler_admission -- --nocapture
cargo test -p skull
cargo test -p bone-query recoverable_syntax_keeps_partial_hints_without_fallback_warnings -- --nocapture
cargo fmt --all -- --check
```

The focused admission suite passed all nine tests. The full skull suite passed
109 unit tests and 1295 integration tests, with one existing ignored integration
test. The selected editor-query test passed once. Formatting passed using the
installed Cargo 1.99.1 and rustfmt. The `cargo +1.99.1` invocation was unavailable
because this environment invokes Cargo directly without rustup.

Three temporary changes to `pipeline.rs` each ran the nine admission tests and
returned test failure. Every mutation was restored before the full crate run.

| Temporary change | Failing tests | What distinguished the defect |
|---|---|---|
| Bypass the initial syntax gate | 2 | Source recovery proceeded into alias resolution, and assembled-module diagnostics lost their required order/count |
| Bypass the expanded-tree syntax gate | 3 | Unknown top-level macro and macro-generated import compiled, and the expression-item macro lost its intended diagnostic |
| Return no pre-expansion macro gate errors | 2 | The macro/function collision compiled, and an unknown label lost its intended diagnostic |

The valid controls passed during all three mutation runs. These checks expose
both acceptance failures and wrong-reason rejection. The two syntax scans serve
different tree states, and the pre-expansion checks consume information expansion
discards. This audit found no production defect or justified consolidation of
those checks. The delivered changes add tests and API documentation.

## Open questions

Other recovery forms, dialect combinations, stdlib failures, incremental
emission and editor features beyond the selected inlay-hint query have further
tests elsewhere and were outside this bounded audit. This work makes no claim
about their complete coverage or performance.
